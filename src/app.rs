//! Core process: one hidden window that owns hotkeys, the tray, the guard and the popups.

use crate::admin;
use crate::audio::{self, Audio, Current, Flow, Roles, WM_APP_DEVICES};
use crate::autostart;
use crate::config::{self, Config, DeviceRef, GuardSide, Hotkey, LoadOutcome};
use crate::devices::{self, DeviceInfo};
use crate::guard::{self, Action, Drift, Guard, Side};
use crate::hotkey::{self, RawHeld};
use crate::i18n::{self, Key, ResolvedLang, fmt, t};
use crate::log;
use crate::popup::{self, Choice, PromptRow, WM_APP_PROMPT_CHOICE};
use crate::stats;
use crate::tray::{self, MenuCmd, Tray, WM_APP_TRAY};
use crate::update;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_PROCESS_LOGON_FLAGS, CreateMutexW, CreateProcessW, CreateProcessWithTokenW,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW,
    GetMessageW, IsWindow, KillTimer, MSG, MSGFLT_ALLOW, PBT_APMRESUMEAUTOMATIC, PM_REMOVE,
    PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW, SetTimer, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_HOTKEY, WM_INPUT, WM_LBUTTONUP,
    WM_POWERBROADCAST, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, PWSTR, w};

/// Posted by the settings process after it saves the config.
pub const WM_APP_RELOAD: u32 = WM_APP + 4;
/// Window class of the core; the admin helper finds the running core by it.
pub const CORE_CLASS: PCWSTR = w!("SwaraCore");

const TIMER_DEVICES: usize = 1;
const DEVICES_MS: u32 = 300;
/// Armed only while a guard pause or snooze runs.
const TIMER_WAKE: usize = 2;
/// Armed only while the tray icon could not be added.
const TIMER_TRAY: usize = 3;
const TRAY_RETRY_MS: u32 = 3_000;
/// About a minute of retries covers a slow shell at logon.
const TRAY_TRIES: u32 = 20;
/// The audio service can still be starting at logon.
const AUDIO_TRIES: u32 = 5;
const AUDIO_RETRY: Duration = Duration::from_secs(2);
const APP_NAME: &str = "Swara";
/// Queue-only tag for a raw key that arrived while the state was busy; never posted.
const RAW_KEY: u32 = WM_APP + 0x100;
/// A tray menu left open while typing must not grow the queue without end.
const BACKLOG_CAP: usize = 1024;
const MISSED_CAP: usize = 4;

struct Core {
    hwnd: HWND,
    lang: ResolvedLang,
    cfg: Config,
    path: Option<PathBuf>,
    /// The file exists but could not be read; saving would overwrite the user's settings.
    save_blocked: bool,
    audio: Audio,
    tray: Tray,
    guard: Guard,
    /// Registered keys; hotkey id = index + 1.
    keys: Vec<Hotkey>,
    /// Valid keys another app already owns; only the Raw Input path can still see them.
    raw_only: Vec<Hotkey>,
    held: RawHeld,
    /// Shared by WM_HOTKEY and WM_INPUT so one press fires once.
    last_fire: Option<Instant>,
    /// Only one prompt fits on screen; the input side waits for the output answer.
    pending: Option<(Side, Drift, Vec<PromptRow>)>,
    /// Drift behind the visible prompt, so "Switch back" reverts exactly what changed.
    shown: Option<(Side, Drift)>,
    tray_tries: u32,
    /// Balloons that came before the tray icon existed; shown once the icon is added.
    missed_notices: VecDeque<String>,
    raw_sink_ok: bool,
    taken_warned: Vec<String>,
}

/// Returns the config, whether saving is blocked, and a notice for the user.
fn load_config(path: Option<&PathBuf>) -> (Config, bool, Option<Key>) {
    let Some(p) = path else {
        log::line("config: no %APPDATA%, running on defaults without saving");
        return (Config::default(), false, None);
    };
    match config::load(p) {
        LoadOutcome::Fresh(mut c) => {
            c.autostart = initial_autostart(true, c.autostart, autostart::is_enabled());
            // Saved now, so the settings window shows the installer's choice too.
            if let Err(e) = config::save(p, &c) {
                log::error(&format!("config: first save failed: {e}"));
            }
            (c, false, None)
        }
        LoadOutcome::Loaded(c) => (c, false, None),
        LoadOutcome::Corrupt(c) => {
            log::line("config: corrupt, renamed to .bad");
            (c, false, Some(Key::ConfigCorrupt))
        }
        LoadOutcome::Unreadable(c) => {
            log::line("config: unreadable, not saving");
            (c, true, Some(Key::ConfigUnreadable))
        }
    }
}

/// First run follows the installer's autostart checkbox (the Run value). Later starts follow
/// config, except that a Run value config calls off was added since by that same checkbox: the
/// core removes it whenever config says off.
fn initial_autostart(fresh: bool, cfg: bool, run_exists: bool) -> bool {
    if fresh { run_exists } else { cfg || run_exists }
}

fn sync_autostart(enabled: bool) {
    if let Err(e) = autostart::set_enabled(enabled) {
        log::error(&format!("autostart: {e}"));
    }
}

fn side_index(side: Side) -> isize {
    match side {
        Side::Out => 0,
        Side::In => 1,
    }
}

impl Core {
    fn save(&self) {
        let Some(p) = self.path.as_ref().filter(|_| !self.save_blocked) else {
            return;
        };
        if let Err(e) = config::save(p, &self.cfg) {
            log::error(&format!("config: save failed: {e}"));
        }
    }

    /// False when the tray icon is missing; the text then waits for the icon.
    fn balloon(&mut self, text: &str) -> bool {
        let shown = self.tray.notice(APP_NAME, text);
        // The earliest notices are kept when full: a config notice outranks later ones.
        let missed = &mut self.missed_notices;
        if !shown && missed.len() < MISSED_CAP && !missed.iter().any(|m| m == text) {
            missed.push_back(text.to_owned());
        }
        shown
    }

    fn show_tray(&mut self) {
        if self.tray.show() {
            self.tray_tries = 0;
            self.disarm(TIMER_TRAY);
            for text in std::mem::take(&mut self.missed_notices) {
                self.balloon(&text);
            }
        } else if self.tray_tries < TRAY_TRIES {
            self.tray_tries += 1;
            self.arm(TIMER_TRAY, TRAY_RETRY_MS);
        } else {
            log::error("tray: giving up until the taskbar restarts");
        }
    }

    /// Returns the labels of keys another app already owns.
    fn register_keys(&mut self) -> Vec<String> {
        let mut taken = Vec::new();
        for k in hotkey::unique_keys(&self.cfg.bindings) {
            if let Err(e) = hotkey::check(&k) {
                log::line(&format!("hotkey: skip {} ({e:?})", hotkey::label(&k)));
                continue;
            }
            if hotkey::passes_through(&k) {
                self.raw_only.push(k);
                continue;
            }
            match hotkey::register(self.hwnd, self.keys.len() as i32 + 1, &k) {
                Ok(()) => self.keys.push(k),
                Err(e) => {
                    log::line(&format!("hotkey: {} failed: {e}", hotkey::label(&k)));
                    taken.push(hotkey::label(&k));
                    self.raw_only.push(k);
                }
            }
        }
        taken
    }

    /// So a key that silently stopped working is not a mystery. Every settings save reloads, so
    /// the same set of keys is reported only once.
    fn warn_keys(&mut self, taken: Vec<String>) {
        if !self.raw_sink_ok || taken == self.taken_warned {
            // Without the sink the start-up notice already covers taken keys.
            return;
        }
        if !taken.is_empty() {
            let text = fmt(
                t(self.lang, Key::HotkeyTaken),
                &[("keys", &taken.join(", "))],
            );
            self.balloon(&text);
        }
        self.taken_warned = taken;
    }

    fn arm(&self, id: usize, ms: u32) {
        // SAFETY: own window; re-arming the same id restarts it. Every core timer is one-shot:
        // core_proc kills it before handling the tick.
        if unsafe { SetTimer(Some(self.hwnd), id, ms, None) } == 0 {
            let why = windows::core::Error::from_thread();
            log::error(&format!("timer {id} not set: {why}"));
        }
    }

    /// Re-checks the guard when a pause or snooze ends; no pause means no timer.
    fn arm_wake(&self) {
        match self.guard.next_wake(Instant::now()) {
            // +1 ms because the guard treats the exact end as still paused.
            Some(left) => self.arm(
                TIMER_WAKE,
                u32::try_from(left.as_millis() + 1).unwrap_or(u32::MAX),
            ),
            None => self.disarm(TIMER_WAKE),
        }
    }

    fn disarm(&self, id: usize) {
        // SAFETY: own window; an id that is not set just makes this fail.
        let _ = unsafe { KillTimer(Some(self.hwnd), id) };
    }

    fn unregister_keys(&mut self) {
        for id in 1..=self.keys.len() {
            hotkey::unregister(self.hwnd, id as i32);
        }
        self.keys.clear();
        self.raw_only.clear();
    }

    fn on_key(&mut self, key: Hotkey) {
        let now = Instant::now();
        if !hotkey::past_dedup(self.last_fire, now) {
            return;
        }
        self.last_fire = Some(now);
        self.wake_stats();
        let list = self.audio.list(Flow::Render);
        let cur = self.audio.default_output();
        let targets = devices::targets_for(key, &self.cfg.bindings);
        match devices::next_target(&targets, &list, cur.as_deref()) {
            Some(d) => self.switch_to(d, Some(key)),
            None => popup::show_osd(
                self.hwnd,
                &format!(
                    "{}\t{}",
                    t(self.lang, Key::OsdNotConnected),
                    hotkey::label(&key)
                ),
            ),
        }
        // A switch can outlast the dedup gap, and the twin message of this press (WM_HOTKEY or
        // WM_INPUT) is handled only after it; measure the gap from the end of the work too.
        self.last_fire = Some(Instant::now());
    }

    fn on_raw(&mut self, vk: u32, down: bool) {
        // Nearly every key is not a hotkey; those skip the modifier queries below.
        if !self.keys.iter().chain(&self.raw_only).any(|k| k.vk == vk) {
            return;
        }
        if !self.held.press(vk, down, Instant::now()) {
            return;
        }
        let mods = hotkey::mods_down();
        let hit = [&self.keys, &self.raw_only]
            .into_iter()
            .find_map(|ks| hotkey::match_raw(vk, mods, ks).map(|i| ks[i]));
        if let Some(k) = hit {
            self.on_key(k);
        }
    }

    fn switch_to(&mut self, d: DeviceInfo, key: Option<Hotkey>) {
        let roles = if self.cfg.also_comms {
            Roles::All
        } else {
            Roles::ConsoleMultimedia
        };
        let name = devices::display_name(&d.name, &d.id, &self.cfg.nicknames);
        let partial = match audio::set_default(&d.id, roles) {
            Ok(()) => false,
            Err(e) => {
                log::error(&format!("switch: set_default failed: {e}"));
                if !guard::switch_landed(&d.id, &self.audio.current().render) {
                    let text = format!("{}\t{name}", t(self.lang, Key::OsdSwitchFailed));
                    popup::show_osd(self.hwnd, &text);
                    return;
                }
                true
            }
        };
        guard::after_switch(
            &mut self.cfg,
            &DeviceRef {
                id: d.id.clone(),
                name: d.name.clone(),
            },
        );
        self.guard.reset();
        self.arm_wake();
        if partial {
            // The roles left behind now differ from the new target; the guard finishes or asks.
            self.schedule_guard_check();
        }
        if self.cfg.osd {
            let text = match key {
                Some(k) => format!("{name}\t{}", hotkey::label(&k)),
                None => name,
            };
            popup::show_osd(self.hwnd, &text);
        }
        // After the OSD: the flush to disk can take a while and the user waits for feedback.
        self.save();
    }

    fn side(&self, side: Side) -> &GuardSide {
        match side {
            Side::Out => &self.cfg.guard_out,
            Side::In => &self.cfg.guard_in,
        }
    }

    fn list(&self, side: Side) -> Vec<DeviceInfo> {
        self.audio.list(match side {
            Side::Out => Flow::Render,
            Side::In => Flow::Capture,
        })
    }

    fn revert(&self, side: Side, d: Drift, list: &[DeviceInfo]) {
        let (def, comms) = guard::targets(self.side(side), list);
        for (on, id, roles) in [
            (d.default_roles, def, Roles::ConsoleMultimedia),
            (d.comms_role, comms, Roles::Comms),
        ] {
            if let (true, Some(id)) = (on, id)
                && let Err(e) = audio::set_default(id, roles)
            {
                log::error(&format!("guard: revert failed: {e}"));
            }
        }
    }

    fn rows(&self, side: Side, d: Drift, roles: &[Option<String>; 3]) -> Vec<PromptRow> {
        let list = self.list(side);
        let name = |id: Option<&str>| {
            id.and_then(|id| list.iter().find(|x| x.id == id))
                .map(|x| devices::display_name(&x.name, &x.id, &self.cfg.nicknames))
                .unwrap_or_default()
        };
        let (def, comms) = guard::targets(self.side(side), &list);
        let mut rows = Vec::new();
        if d.default_roles {
            // Show the role that actually moved away from the pick.
            let now = roles[..2]
                .iter()
                .flatten()
                .find(|r| Some(r.as_str()) != def);
            rows.push(PromptRow {
                now_label: Key::PopupNow,
                now: name(now.map(String::as_str)),
                pick: name(def),
            });
        }
        if d.comms_role {
            rows.push(PromptRow {
                now_label: Key::CommsDevice,
                now: name(roles[2].as_deref()),
                pick: name(comms),
            });
        }
        rows
    }

    fn show_prompt(&mut self, side: Side, d: Drift, rows: Vec<PromptRow>) {
        if let Err(e) = popup::show_prompt(self.hwnd, self.lang, side, &rows, self.cfg.snooze_min) {
            log::error(&format!("guard: prompt failed: {e}"));
            let heading = match side {
                Side::Out => Key::PopupOutputChanged,
                Side::In => Key::PopupInputChanged,
            };
            // Without the balloon either, the user has not been told; ask again next event.
            if !self.balloon(t(self.lang, heading)) {
                self.guard.forget_notified();
            }
            // The other side would otherwise wait for a prompt that never comes; each try takes
            // it off the queue, so this ends.
            if let Some((side, d, rows)) = self.pending.take() {
                self.show_prompt(side, d, rows);
            }
            return;
        }
        self.shown = Some((side, d));
        self.tray.set_alert(true, APP_NAME);
    }

    /// Drops prompts the guard no longer needs, e.g. the user made the current device the target.
    fn drop_stale_prompts(&mut self, cur: &Current, outs: &[DeviceInfo], ins: &[DeviceInfo]) {
        let stale = |cfg: &Config, s: Side| {
            guard::prompt_stale(cfg, s, cur, if s == Side::Out { outs } else { ins })
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|(s, _, _)| stale(&self.cfg, *s))
        {
            self.pending = None;
        }
        if let Some((s, _)) = self.shown
            && stale(&self.cfg, s)
        {
            popup::hide_prompt();
            self.shown = None;
            self.tray.set_alert(false, APP_NAME);
            if let Some((side, d, rows)) = self.pending.take() {
                self.show_prompt(side, d, rows);
            }
        }
    }

    fn guard_check(&mut self) {
        // The guard ignores unwatched sides, so their lists are never built.
        let watched = [Side::Out, Side::In].map(|s| guard::watches(&self.cfg, s));
        let cur = if watched.contains(&true) {
            self.audio.current()
        } else {
            Current::default()
        };
        let list_if = |on: bool, side| if on { self.list(side) } else { Vec::new() };
        let outs = list_if(watched[0], Side::Out);
        let ins = list_if(watched[1], Side::In);
        self.drop_stale_prompts(&cur, &outs, &ins);
        match self
            .guard
            .evaluate(&self.cfg, &cur, &outs, &ins, Instant::now())
        {
            Action::None => {}
            Action::Revert { out, inp } => {
                log::line(&format!("guard: revert out={out:?} in={inp:?}"));
                if let Some(d) = out {
                    self.revert(Side::Out, d, &outs);
                }
                if let Some(d) = inp {
                    self.revert(Side::In, d, &ins);
                }
            }
            Action::Notify { out, inp } => {
                log::line(&format!("guard: notify out={out:?} in={inp:?}"));
                let out = out.map(|d| (Side::Out, d, self.rows(Side::Out, d, &cur.render)));
                let inp = inp.map(|d| (Side::In, d, self.rows(Side::In, d, &cur.capture)));
                let (first, rest) = match out {
                    Some(o) => (Some(o), inp),
                    None => (inp, None),
                };
                self.pending = rest;
                if let Some((side, d, rows)) = first {
                    self.show_prompt(side, d, rows);
                }
            }
            Action::GiveUp { announce } => {
                log::line("guard: giving up");
                let n = (guard::BACKOFF.as_secs() / 60).to_string();
                let body = fmt(t(self.lang, Key::GuardPaused), &[("n", &n)]);
                // Focus Assist and full-screen games hide balloons but not the OSD; once per
                // fight is enough over a game.
                if announce {
                    popup::show_osd(self.hwnd, &body);
                }
                self.balloon(&body);
            }
        }
        self.arm_wake();
    }

    fn on_choice(&mut self, wp: usize, lp: isize) {
        let Some(choice) = Choice::from_index(wp) else {
            return;
        };
        let side = if lp == side_index(Side::In) {
            Side::In
        } else {
            Side::Out
        };
        match choice {
            Choice::SwitchBack => {
                if let Some((_, d)) = self.shown.filter(|(s, _)| *s == side) {
                    let list = self.list(side);
                    self.revert(side, d, &list);
                }
            }
            Choice::Keep => {
                let cur = self.audio.current();
                let list = self.list(side);
                let (s, roles) = match side {
                    Side::Out => (&mut self.cfg.guard_out, &cur.render),
                    Side::In => (&mut self.cfg.guard_in, &cur.capture),
                };
                guard::keep(s, roles, &list);
                self.save();
            }
            Choice::Snooze => {
                let mins = Duration::from_secs(u64::from(self.cfg.snooze_min) * 60);
                self.guard.snooze(side, Instant::now() + mins);
                self.arm_wake();
            }
        }
        self.shown = None;
        self.tray.set_alert(false, APP_NAME);
        if let Some((side, d, rows)) = self.pending.take() {
            self.show_prompt(side, d, rows);
        }
    }

    fn tray_event(&mut self, lp: LPARAM) {
        let event = (lp.0 as u32) & 0xFFFF;
        if event == WM_LBUTTONUP {
            self.open_settings();
        } else if event == WM_CONTEXTMENU {
            self.tray_menu();
        }
    }

    fn tray_menu(&mut self) {
        let active = self.audio.default_output();
        let mut outs = self.audio.list(Flow::Render);
        outs.retain(|d| d.present);
        let mut entries = Vec::with_capacity(outs.len());
        for d in &outs {
            let label = devices::display_name(&d.name, &d.id, &self.cfg.nicknames);
            // Doubled so the menu shows a literal & instead of underlining the next letter.
            let checked = active.as_deref() == Some(d.id.as_str());
            entries.push((label.replace('&', "&&"), checked));
        }
        let picked = self
            .tray
            .menu(self.hwnd, self.lang, &entries, self.cfg.guard_enabled);
        let Some(cmd) = picked else {
            return;
        };
        match cmd {
            MenuCmd::Switch(i) => {
                if let Some(d) = outs.get(i).cloned() {
                    self.switch_to(d, None);
                }
            }
            MenuCmd::ToggleGuard => {
                self.cfg.guard_enabled ^= true;
                self.save();
                self.guard.reset();
                self.arm_wake();
                // Turning it on catches up with drift; turning it off drops a stale prompt.
                self.schedule_guard_check();
            }
            MenuCmd::Settings => self.open_settings(),
            MenuCmd::Quit => self.shut_down(),
        }
    }

    fn open_settings(&mut self) {
        if !open_settings_process() {
            self.balloon(t(self.lang, Key::SettingsNotOpened));
        }
    }

    /// The daily ping has no timer; user and device events check it instead.
    fn wake_stats(&self) {
        stats::wake(&self.cfg, self.lang);
    }

    fn schedule_guard_check(&self) {
        self.arm(TIMER_DEVICES, DEVICES_MS);
    }

    fn reload(&mut self) {
        let before = (
            self.cfg.guard_enabled,
            self.cfg.guard_out.clone(),
            self.cfg.guard_in.clone(),
        );
        let (cfg, blocked, note) = load_config(self.path.as_ref());
        // A transient read error must not wipe the bindings in use.
        if !blocked {
            self.cfg = cfg;
        }
        self.save_blocked = blocked;
        self.lang = i18n::current(self.cfg.lang.clone());
        if let Some(k) = note {
            self.balloon(t(self.lang, k));
        }
        sync_autostart(self.cfg.autostart);
        self.unregister_keys();
        let taken = self.register_keys();
        self.warn_keys(taken);
        // A new guard target may already differ from Windows; check now instead of waiting for
        // the next device event. Other saves keep snoozes and the notified state.
        let after = (
            self.cfg.guard_enabled,
            self.cfg.guard_out.clone(),
            self.cfg.guard_in.clone(),
        );
        if before != after {
            self.guard.reset();
            self.arm_wake();
            let cur = self.audio.current();
            for (side, old) in [(Side::Out, &before.1), (Side::In, &before.2)] {
                let list = self.list(side);
                if let Some(d) = guard::on_new_target(&self.cfg, old, side, &cur, &list) {
                    log::line(&format!("guard: new target {side:?}, switching"));
                    self.revert(side, d, &list);
                }
            }
            self.schedule_guard_check();
        }
        log::line("config: reloaded");
        self.wake_stats();
    }

    fn shut_down(&mut self) {
        self.unregister_keys();
        self.tray.remove();
        popup::hide_all();
        // SAFETY: no arguments to check; it only ends this thread's message loop, and run()
        // drops Audio after that.
        unsafe { PostQuitMessage(0) }
    }

    fn route(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        match msg {
            WM_HOTKEY => {
                // Hotkey ids start at 1, so id 0 wraps to an index that never exists.
                if let Some(&key) = self.keys.get(wp.0.wrapping_sub(1)) {
                    self.on_key(key);
                }
            }
            WM_APP_DEVICES => self.schedule_guard_check(),
            WM_POWERBROADCAST if wp.0 == PBT_APMRESUMEAUTOMATIC as usize => self.wake_stats(),
            WM_TIMER if wp.0 == TIMER_DEVICES => {
                // Once per settled device storm instead of once per event.
                self.wake_stats();
                self.guard_check();
            }
            WM_TIMER if wp.0 == TIMER_WAKE => self.guard_check(),
            WM_TIMER if wp.0 == TIMER_TRAY => self.show_tray(),
            RAW_KEY => self.on_raw(wp.0 as u32, lp.0 != 0),
            WM_APP_PROMPT_CHOICE => self.on_choice(wp.0, lp.0),
            WM_APP_TRAY => self.tray_event(lp),
            WM_APP_RELOAD => self.reload(),
            WM_CLOSE => self.shut_down(),
            other if other == taskbar_created() => {
                // A (re)started taskbar earns a fresh round of retries.
                self.tray_tries = 0;
                self.show_tray();
                // An elevated core that started before the shell could not read the config as
                // the user; the shell is up now.
                if self.save_blocked {
                    self.reload();
                }
            }
            _ => {}
        }
    }
}

fn taskbar_created() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    *ID.get_or_init(tray::taskbar_created_msg)
}

/// An elevated core starts settings with the shell's unelevated token, and never with its own:
/// false means that failed and settings did not open.
fn open_settings_process() -> bool {
    let from_shell = admin::elevated_split();
    let program = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            log::error(&format!("settings: exe path unknown: {e}"));
            return !from_shell;
        }
    };
    let mut cmd: Vec<u16> = format!("\"{}\" --settings", program.display())
        .encode_utf16()
        .chain([0])
        .collect();
    let si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let r = if from_shell {
        admin::unelevated_shell_token().and_then(|token| {
            // SAFETY: `token` is an open primary token, `cmd` a NUL-terminated mutable buffer,
            // and `si`/`pi` outlive the call. Nothing is inherited.
            unsafe {
                CreateProcessWithTokenW(
                    token.0,
                    CREATE_PROCESS_LOGON_FLAGS(0),
                    PCWSTR::null(),
                    Some(PWSTR(cmd.as_mut_ptr())),
                    PROCESS_CREATION_FLAGS(0),
                    None,
                    PCWSTR::null(),
                    &si,
                    &mut pi,
                )
            }
            .map_err(|e| e.to_string())
        })
    } else {
        // SAFETY: `cmd` is a NUL-terminated mutable buffer, `si`/`pi` outlive the call, and no
        // handles are inherited so the child cannot keep our mutex alive.
        unsafe {
            CreateProcessW(
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(0),
                None,
                None,
                &si,
                &mut pi,
            )
        }
        .map_err(|e| e.to_string())
    };
    match r {
        // SAFETY: both handles were just returned to us and are closed once.
        Ok(()) => unsafe {
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        },
        Err(e) => {
            log::error(&format!("settings: spawn failed: {e}"));
            return !from_shell;
        }
    }
    true
}

/// A message that arrived while the core was borrowed, kept to replay afterwards.
type Queued = (u32, WPARAM, LPARAM);

thread_local! {
    static CORE: RefCell<Option<Core>> = const { RefCell::new(None) };
    /// Fills up while the core is borrowed, for example during the tray menu's modal loop.
    static BACKLOG: RefCell<VecDeque<Queued>> = const { RefCell::new(VecDeque::new()) };
}

/// Drops a key-down when the last queued event of that key is a key-down too: auto-repeat while
/// the tray menu is open adds nothing. Past the cap further keys are dropped (a lost key-up heals
/// itself in `RawHeld`); other messages are rare and always kept, WM_CLOSE among them.
fn queue_later(backlog: &mut VecDeque<Queued>, q: Queued) {
    let (msg, wp, lp) = q;
    if msg != RAW_KEY {
        backlog.push_back(q);
        return;
    }
    let repeat = lp.0 != 0
        && backlog
            .iter()
            .rev()
            .find(|e| e.0 == RAW_KEY && e.1 == wp)
            .is_some_and(|e| e.2.0 != 0);
    if !repeat && backlog.len() < BACKLOG_CAP {
        backlog.push_back(q);
    }
}

fn next_queued() -> Option<Queued> {
    BACKLOG.with_borrow_mut(VecDeque::pop_front)
}

/// Runs `work`, then replays whatever queued up during it. If the core is already borrowed,
/// `later` joins the queue instead. False only before the core exists.
fn with_core(later: Option<Queued>, work: impl FnOnce(&mut Core)) -> bool {
    CORE.with(|slot| match slot.try_borrow_mut() {
        Err(_) => {
            if let Some(q) = later {
                BACKLOG.with_borrow_mut(|backlog| queue_later(backlog, q));
            }
            true
        }
        Ok(mut held) => {
            let Some(core) = held.as_mut() else {
                return false;
            };
            work(core);
            while let Some((msg, wp, lp)) = next_queued() {
                core.route(msg, wp, lp);
            }
            true
        }
    })
}

const ROUTED: [u32; 8] = [
    WM_HOTKEY,
    WM_POWERBROADCAST,
    WM_TIMER,
    WM_CLOSE,
    WM_APP_DEVICES,
    WM_APP_PROMPT_CHOICE,
    WM_APP_TRAY,
    WM_APP_RELOAD,
];

fn core_handles(msg: u32) -> bool {
    ROUTED.contains(&msg) || (msg != 0 && msg == taskbar_created())
}

unsafe extern "system" fn core_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TIMER {
        // Core timers are one-shot. Killing the timer first means a busy core queues one tick,
        // not one per period.
        // SAFETY: our own window and the id Windows just delivered.
        let _ = unsafe { KillTimer(Some(hwnd), wp.0) };
    }
    if msg == WM_INPUT {
        // Queued like the rest when the core is busy: dropping a key-up would leave the key
        // stuck as held.
        if let Some((vk, down)) = hotkey::raw_key_event(lp) {
            let later = (RAW_KEY, WPARAM(vk as usize), LPARAM(isize::from(down)));
            with_core(Some(later), |core| core.on_raw(vk, down));
        }
    } else if core_handles(msg) && with_core(Some((msg, wp, lp)), |core| core.route(msg, wp, lp)) {
        // 0 for a handled WM_CLOSE, so DefWindowProc does not destroy the window first.
        return LRESULT(0);
    }
    // SAFETY: the arguments Windows passed in, unchanged.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

const CLASS_SIZE: u32 = size_of::<WNDCLASSEXW>() as u32;

fn make_window() -> windows::core::Result<HWND> {
    // SAFETY: a null name asks for this exe's own module; nothing needs freeing.
    let module = unsafe { GetModuleHandleW(None) }?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSEXW {
        cbSize: CLASS_SIZE,
        lpfnWndProc: Some(core_proc),
        hInstance: instance,
        lpszClassName: CORE_CLASS,
        ..Default::default()
    };
    // SAFETY: `class` is fully initialised and its strings are static.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return Err(windows::core::Error::from_thread());
    }
    // A real top-level window that is never shown: message-only windows do not get the
    // TaskbarCreated broadcast.
    // SAFETY: the class was registered above; all strings are static.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CORE_CLASS,
            w!("Swara"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
}

/// Waits while still answering messages: a broadcast sent to an unresponsive top-level window
/// would hang its sender. Only used on the failing start path.
fn wait_pumping(total: Duration) {
    let end = Instant::now() + total;
    let mut msg = MSG::default();
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: drains this thread's queue; with no core yet, core_proc defers to
        // DefWindowProc.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Runs the tray core until Quit. Returns the process exit code.
pub fn run() -> i32 {
    // SAFETY: static name; the handle stays open for the process lifetime on purpose.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\Swara-Core")) };
    // SAFETY: reads this thread's last error right after CreateMutexW.
    if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // A second launch often means the icon went missing; the running core re-adds it.
        // SAFETY: static class name; posting to a window that is gone just fails.
        unsafe {
            if let Ok(core) = FindWindowW(CORE_CLASS, None) {
                let _ = PostMessageW(Some(core), taskbar_created(), WPARAM(0), LPARAM(0));
            }
        }
        return 0;
    }
    log::init();
    // SAFETY: once on this thread, before any COM use.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    log::line(&format!("start v{}", env!("CARGO_PKG_VERSION")));
    stats::init_core();

    let path = config::config_file();
    let (mut cfg, save_blocked, note) = load_config(path.as_ref());
    // Only at start: on a reload after "off" the Run value is simply not removed yet.
    if !save_blocked
        && initial_autostart(false, cfg.autostart, autostart::is_enabled()) != cfg.autostart
    {
        cfg.autostart = true;
        if let Some(Err(e)) = path.as_ref().map(|p| config::save(p, &cfg)) {
            log::error(&format!("config: autostart save failed: {e}"));
        }
    }
    sync_autostart(cfg.autostart);
    // Audio posts device events to this window, so the window comes first.
    let hwnd = match make_window() {
        Ok(h) => h,
        Err(e) => {
            log::error(&format!("core window not created: {e}"));
            return 1;
        }
    };
    // UIPI drops these when the core is elevated: the unelevated settings process sends
    // the reload, and Explorer broadcasts TaskbarCreated. The tray callback stays filtered.
    for m in [WM_APP_RELOAD, taskbar_created()] {
        // SAFETY: own window, created above on this thread.
        if let Err(e) = unsafe { ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None) } {
            log::error(&format!("message filter failed: {e}"));
        }
    }
    let mut tries = 0;
    let audio = loop {
        match Audio::new(hwnd) {
            Ok(a) => break a,
            Err(e) if tries + 1 < AUDIO_TRIES => {
                tries += 1;
                log::line(&format!("audio init failed, retrying: {e}"));
                wait_pumping(AUDIO_RETRY);
                // A WM_CLOSE while waiting destroys the window; running on without it would
                // hold the mutex with no tray, hotkeys or sink.
                // SAFETY: handle check only.
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    log::line("closed while waiting for audio");
                    return 1;
                }
            }
            Err(e) => {
                log::error(&format!("audio init failed: {e}"));
                return 1;
            }
        }
    };
    let raw_sink_ok = match hotkey::register_raw_sink(hwnd) {
        Ok(()) => true,
        Err(e) => {
            log::error(&format!("raw input sink failed: {e}"));
            false
        }
    };
    let mut core = Core {
        hwnd,
        lang: i18n::current(cfg.lang.clone()),
        cfg,
        path,
        save_blocked,
        audio,
        tray: Tray::create(hwnd),
        guard: Guard::new(),
        keys: Vec::new(),
        raw_only: Vec::new(),
        held: RawHeld::default(),
        last_fire: None,
        pending: None,
        shown: None,
        tray_tries: 0,
        missed_notices: VecDeque::new(),
        raw_sink_ok,
        taken_warned: Vec::new(),
    };
    core.show_tray();
    let taken = core.register_keys();
    if let Some(k) = note {
        core.balloon(t(core.lang, k));
    }
    if raw_sink_ok {
        core.warn_keys(taken);
    } else {
        core.balloon(t(core.lang, Key::KeyboardUnreadable));
    }
    // Windows may have drifted while Swara was not running.
    core.schedule_guard_check();
    core.wake_stats();
    // A big %TEMP% must not hold up the first hotkey.
    let sweep = std::thread::Builder::new().spawn(update::remove_stale_installers);
    if let Err(e) = sweep {
        log::error(&format!("installer sweep not started: {e}"));
    }
    CORE.set(Some(core));

    let mut msg = MSG::default();
    loop {
        // SAFETY: the usual message pump, on the thread that owns the window.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // 0 is WM_QUIT and -1 an error; both end the loop.
        if got.0 <= 0 {
            break;
        }
        // SAFETY: `msg` was just filled by GetMessageW.
        unsafe {
            _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(CORE.take());
    log::line("exit");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vk: usize, down: bool) -> Queued {
        (RAW_KEY, WPARAM(vk), LPARAM(isize::from(down)))
    }

    #[test]
    fn backlog_skips_repeats_and_stays_bounded() {
        let mut q = VecDeque::new();
        queue_later(&mut q, key(0x6B, true));
        queue_later(&mut q, key(0x6B, true));
        queue_later(&mut q, key(0x4B, true));
        queue_later(&mut q, key(0x6B, true));
        assert_eq!(q.len(), 2);
        queue_later(&mut q, key(0x6B, false));
        queue_later(&mut q, key(0x6B, true));
        assert_eq!(q.len(), 4);
        for n in 0..(2 * BACKLOG_CAP) {
            queue_later(&mut q, key(n % 2, true));
            queue_later(&mut q, key(n % 2, false));
        }
        assert_eq!(q.len(), BACKLOG_CAP);
        // Messages other than keys are never dropped.
        queue_later(&mut q, (WM_CLOSE, WPARAM(0), LPARAM(0)));
        assert_eq!(q.back().map(|e| e.0), Some(WM_CLOSE));
    }

    #[test]
    fn first_run_follows_installer_choice() {
        assert!(initial_autostart(true, false, true));
        assert!(!initial_autostart(true, true, false));
        assert!(initial_autostart(false, false, true));
        assert!(!initial_autostart(false, false, false));
        assert!(initial_autostart(false, true, false));
    }
}
