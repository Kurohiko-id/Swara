//! Core process: one hidden window that owns hotkeys, the tray, the guard and the popups.

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
    CreateMutexW, CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    KillTimer, MSG, MSGFLT_ALLOW, PBT_APMRESUMEAUTOMATIC, PostQuitMessage, RegisterClassExW,
    SetTimer, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_HOTKEY,
    WM_INPUT, WM_LBUTTONUP, WM_POWERBROADCAST, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, PWSTR, w};

/// Posted by the settings process after it saves the config.
pub const WM_APP_RELOAD: u32 = WM_APP + 4;
/// Window class of the core; the admin helper finds the running core by it.
pub const CORE_CLASS: PCWSTR = w!("SwaraCore");

const TIMER_DEVICES: usize = 1;
const DEVICES_MS: u32 = 300;
const APP_NAME: &str = "Swara";
/// Queue-only tag for a raw key that arrived while the state was busy; never posted.
const RAW_KEY: u32 = WM_APP + 0x100;

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

/// First run follows the installer's autostart checkbox (the Run value); later runs follow config.
fn initial_autostart(fresh: bool, cfg: bool, run_exists: bool) -> bool {
    if fresh { run_exists } else { cfg }
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

    fn balloon(&self, key: Key) {
        self.tray.notice(APP_NAME, t(self.lang, key));
    }

    fn register_keys(&mut self) {
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
                    self.raw_only.push(k);
                }
            }
        }
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
        let cur = self.audio.current().render[0].clone();
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
    }

    fn on_raw(&mut self, vk: u32, down: bool) {
        if !self.held.press(vk, down) {
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
        if let Err(e) = audio::set_default(&d.id, roles) {
            log::error(&format!("switch: set_default failed: {e}"));
            return;
        }
        guard::after_switch(
            &mut self.cfg,
            &DeviceRef {
                id: d.id.clone(),
                name: d.name.clone(),
            },
        );
        self.save();
        self.guard.reset();
        if self.cfg.osd {
            let name = devices::display_name(&d.name, &d.id, &self.cfg.nicknames);
            let text = match key {
                Some(k) => format!("{name}\t{}", hotkey::label(&k)),
                None => name,
            };
            popup::show_osd(self.hwnd, &text);
        }
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
        let cur = self.audio.current();
        let outs = self.audio.list(Flow::Render);
        let ins = self.audio.list(Flow::Capture);
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
            Action::GiveUp => {
                log::line("guard: giving up");
                let n = (guard::BACKOFF.as_secs() / 60).to_string();
                let body = fmt(t(self.lang, Key::GuardPaused), &[("n", &n)]);
                self.tray.notice(APP_NAME, &body);
            }
        }
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
            open_settings_process();
        } else if event == WM_CONTEXTMENU {
            self.tray_menu();
        }
    }

    fn tray_menu(&mut self) {
        let active = self.audio.current().render[0].clone();
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
            }
            MenuCmd::Settings => open_settings_process(),
            MenuCmd::Quit => self.shut_down(),
        }
    }

    /// The daily ping has no timer; user and device events check it instead.
    fn wake_stats(&self) {
        stats::wake(&self.cfg, self.lang);
    }

    fn schedule_guard_check(&self) {
        // SAFETY: own window; re-arming the same id restarts the one-shot debounce.
        unsafe { SetTimer(Some(self.hwnd), TIMER_DEVICES, DEVICES_MS, None) };
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
            self.balloon(k);
        }
        sync_autostart(self.cfg.autostart);
        self.unregister_keys();
        self.register_keys();
        // A new guard target may already differ from Windows; check now instead of waiting for
        // the next device event. Other saves keep snoozes and the notified state.
        let after = (
            self.cfg.guard_enabled,
            self.cfg.guard_out.clone(),
            self.cfg.guard_in.clone(),
        );
        if before != after {
            self.guard.reset();
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
            WM_APP_DEVICES => {
                self.schedule_guard_check();
                self.wake_stats();
            }
            WM_POWERBROADCAST if wp.0 == PBT_APMRESUMEAUTOMATIC as usize => self.wake_stats(),
            WM_TIMER if wp.0 == TIMER_DEVICES => self.guard_check(),
            RAW_KEY => self.on_raw(wp.0 as u32, lp.0 != 0),
            WM_APP_PROMPT_CHOICE => self.on_choice(wp.0, lp.0),
            WM_APP_TRAY => self.tray_event(lp),
            WM_APP_RELOAD => self.reload(),
            WM_CLOSE => self.shut_down(),
            other if other == taskbar_created() => self.tray.show(),
            _ => {}
        }
    }
}

fn taskbar_created() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    *ID.get_or_init(tray::taskbar_created_msg)
}

fn open_settings_process() {
    let program = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            log::error(&format!("settings: exe path unknown: {e}"));
            return;
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
    // SAFETY: `cmd` is a NUL-terminated mutable buffer, `si`/`pi` outlive the call, and no
    // handles are inherited so the child cannot keep our mutex alive.
    let r = unsafe {
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
    };
    match r {
        // SAFETY: both handles were just returned to us and are closed once.
        Ok(()) => unsafe {
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        },
        Err(e) => log::error(&format!("settings: spawn failed: {e}")),
    }
}

/// A message that arrived while the core was borrowed, kept to replay afterwards.
type Queued = (u32, WPARAM, LPARAM);

thread_local! {
    static CORE: RefCell<Option<Core>> = const { RefCell::new(None) };
    /// Fills up while the core is borrowed, for example during the tray menu's modal loop.
    static BACKLOG: RefCell<VecDeque<Queued>> = const { RefCell::new(VecDeque::new()) };
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
                BACKLOG.with_borrow_mut(|backlog| backlog.push_back(q));
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

/// Runs the tray core until Quit. Returns the process exit code.
pub fn run() -> i32 {
    // SAFETY: static name; the handle stays open for the process lifetime on purpose.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\Swara-Core")) };
    // SAFETY: reads this thread's last error right after CreateMutexW.
    if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }
    log::init();
    // SAFETY: once on this thread, before any COM use.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    log::line(&format!("start v{}", env!("CARGO_PKG_VERSION")));
    stats::init_core();

    let path = config::config_file();
    let (cfg, save_blocked, note) = load_config(path.as_ref());
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
    let audio = match Audio::new(hwnd) {
        Ok(a) => a,
        Err(e) => {
            log::error(&format!("audio init failed: {e}"));
            return 1;
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
    };
    core.register_keys();
    if let Some(e) = hotkey::register_raw_sink(hwnd).err() {
        log::error(&format!("raw input sink failed: {e}"));
    }
    if let Some(k) = note {
        core.balloon(k);
    }
    // Windows may have drifted while Swara was not running.
    core.schedule_guard_check();
    core.wake_stats();
    update::remove_stale_installers();
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
    use super::initial_autostart;

    #[test]
    fn first_run_follows_installer_choice() {
        assert!(initial_autostart(true, false, true));
        assert!(!initial_autostart(true, true, false));
        assert!(!initial_autostart(false, false, true));
        assert!(initial_autostart(false, true, false));
    }
}
