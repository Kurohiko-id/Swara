//! Settings window state, model building and the save path.

use super::drag;
use super::record::{self, SinkEvent};
use crate::audio::{Audio, Flow};
use crate::config::{
    self, Binding, Config, DeviceRef, GuardMode, GuardSide, Hotkey, Lang, LoadOutcome,
};
use crate::devices::{self, DeviceInfo};
use crate::i18n::{self, Key, ResolvedLang, fmt, t};
use crate::{admin, app, hotkey, log, update};
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, SW_HIDE, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, w};

slint::include_modules!();

pub const WINDOW_CLASS: PCWSTR = w!("SwaraSettings");
const WINDOW_CLASS_STR: &str = "SwaraSettings";
const LINKS: [&str; 4] = [
    "https://ko-fi.com/kurohikoid",
    "https://saweria.co/Kurohiko",
    "https://kurohikotopup.com",
    "https://github.com/Kurohiko-id/Swara/issues",
];
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const EXEC_INFO_BYTES: u32 = size_of::<SHELLEXECUTEINFOW>() as u32;
const DAY_SECS: u64 = 86_400;
const RESIZE_SETTLE: Duration = Duration::from_millis(50);
// Must match `SettingsView` width/height in ui/settings.slint.
const WINDOW_W: f32 = 760.0;
const WINDOW_H: f32 = 488.0;
const GUARD_MODES: [GuardMode; 3] = [GuardMode::Off, GuardMode::Notify, GuardMode::AutoRevert];
const LANG_CHOICES: [Lang; 3] = [Lang::Auto, Lang::Id, Lang::En];

enum Upd {
    Idle,
    Checking,
    Latest,
    Available(update::Release),
    Failed,
    Downloading,
    InstallFailed,
}

struct State {
    w: slint::Weak<SettingsView>,
    cfg: Config,
    lang: ResolvedLang,
    path: Option<PathBuf>,
    /// config.json was unreadable at open: the controls show defaults, so saving would
    /// overwrite the user's real settings.
    blocked: bool,
    touched: config::Touched,
    audio: Option<Audio>,
    sink: HWND,
    out_opts: Vec<DeviceInfo>,
    in_opts: Vec<DeviceInfo>,
    name_ids: Vec<String>,
    /// Row being recorded, and which key: `Some(j)` re-records key `j`, `None` adds a key.
    recording: Option<(usize, Option<usize>)>,
    row_err: Option<(usize, Key)>,
    error: Option<Key>,
    upd: Upd,
    admin_ok: bool,
    admin_busy: bool,
    admin_failed: bool,
    save_timer: Timer,
}

thread_local! {
    static ST: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State, &SettingsView) -> R) -> Option<R> {
    ST.with(|c| {
        let mut b = c.borrow_mut();
        let st = b.as_mut()?;
        let w = st.w.upgrade()?;
        Some(f(st, &w))
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn strings(v: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    model(v.into_iter().map(SharedString::from).collect())
}

fn apply_strings(w: &SettingsView, l: ResolvedLang) {
    let labels = w.global::<Strings>();
    macro_rules! set {
        ($($setter:ident => $k:ident),* $(,)?) => { $( labels.$setter(t(l, Key::$k).into()); )* };
    }
    set! {
        set_keys_tab => TabKeys, set_guard_tab => TabGuard, set_general_tab => GeneralTab,
        set_keys_help => KeysHelp, set_add_key => AddKey, set_set_key => SetKey,
        set_press_key => PressKey, set_drag_hint => DragHint,
        set_remove_row => RemoveRow, set_remove_key => RemoveKey, set_rerecord_key => RerecordKey,
        set_add_another_key => AddAnotherKey, set_on_switch => OnSwitch, set_show_name => ShowName,
        set_show_name_sub => ShowNameSub, set_switch_comms => SwitchComms,
        set_switch_comms_sub => SwitchCommsSub, set_device_names => DeviceNames,
        set_guard_on => GuardOn, set_guard_on_sub => GuardOnSub, set_output => Output,
        set_input => Input, set_main_device => MainDevice, set_comms_device => CommsDevice,
        set_mode => Mode, set_snooze_label => SnoozeLabel, set_snooze_sub => SnoozeSub,
        set_minutes => Minutes, set_decrease => Decrease, set_increase => Increase,
        set_language_title => Language, set_system => System, set_run_at_startup => RunAtStartup,
        set_run_as_admin => RunAsAdmin, set_update_support => UpdateSupport,
        set_kofi_row => KofiRow, set_kofi => Kofi, set_saweria_row => SaweriaRow,
        set_saweria => Saweria, set_topup_row => TopUpRow, set_topup => TopUp,
        set_bug_row => BugRow, set_bug => Bug,
        set_pick_device => TrayDevices,
    }
    let list = |ks: [Key; 3]| strings(ks.map(|k| t(l, k).to_string()));
    labels.set_modes(list([Key::ModeOff, Key::ModeNotify, Key::ModeAuto]));
    labels.set_langs(list([Key::LangAuto, Key::LangIndonesian, Key::LangEnglish]));
    // The first sentence is shown bold; Slint's software renderer has no inline styles.
    let note = t(l, Key::LeagueNote);
    let (bold, rest) = note
        .split_once("? ")
        .map_or((String::new(), note), |(a, b)| (format!("{a}?"), b));
    labels.set_league_bold(bold.into());
    labels.set_league_rest(rest.into());
    let (pre, post) = t(l, Key::Version)
        .split_once("{ver}")
        .unwrap_or((t(l, Key::Version), ""));
    labels.set_version_prefix(pre.trim().into());
    labels.set_version_suffix(post.trim().into());
}

/// The enumerated list plus every referenced device that is not in it, shown as not connected.
fn options<'a>(list: &[DeviceInfo], refs: impl Iterator<Item = &'a DeviceRef>) -> Vec<DeviceInfo> {
    let mut out = list.to_vec();
    for r in refs {
        if !r.id.is_empty() && devices::resolve(r, &out).is_none() {
            out.push(DeviceInfo {
                id: r.id.clone(),
                name: r.name.clone(),
                present: false,
            });
        }
    }
    out
}

fn index_of(r: &DeviceRef, opts: &[DeviceInfo]) -> i32 {
    if r.id.is_empty() {
        return -1;
    }
    devices::resolve(r, opts)
        .and_then(|d| opts.iter().position(|o| o.id == d.id))
        .map_or(-1, |i| i as i32)
}

fn dev_ref(d: &DeviceInfo) -> DeviceRef {
    DeviceRef {
        id: d.id.clone(),
        name: d.name.clone(),
    }
}

impl State {
    fn opts(&self, l: &[DeviceInfo]) -> Vec<Opt> {
        l.iter()
            .map(|d| {
                let name = devices::display_name(&d.name, &d.id, &self.cfg.nicknames);
                Opt {
                    label: if d.present {
                        name.into()
                    } else {
                        fmt(t(self.lang, Key::DeviceNotConnected), &[("name", &name)]).into()
                    },
                    enabled: d.present,
                }
            })
            .collect()
    }

    fn refresh_devices(&mut self) {
        let (outs, ins) = self.audio.as_ref().map_or((Vec::new(), Vec::new()), |a| {
            (a.list(Flow::Render), a.list(Flow::Capture))
        });
        self.name_ids = outs.iter().chain(&ins).map(|d| d.id.clone()).collect();
        let g = &self.cfg;
        self.out_opts = options(
            &outs,
            g.bindings
                .iter()
                .map(|b| &b.device)
                .chain(g.guard_out.default.iter())
                .chain(g.guard_out.comms.iter()),
        );
        self.in_opts = options(
            &ins,
            g.guard_in.default.iter().chain(g.guard_in.comms.iter()),
        );
    }

    fn render_names(&self, w: &SettingsView) {
        let names = self
            .out_opts
            .iter()
            .chain(&self.in_opts)
            .filter(|d| self.name_ids.contains(&d.id))
            .map(|d| NameRow {
                label: d.name.clone().into(),
                nick: self
                    .cfg
                    .nicknames
                    .get(&d.id)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
            })
            .collect();
        w.set_names(model(names));
    }

    fn render_binds(&self, w: &SettingsView) {
        let b = &self.cfg.bindings;
        let rows = b
            .iter()
            .enumerate()
            .map(|(i, r)| {
                // The cycle tag follows the row's first key.
                let first = r.keys.first().copied();
                let same: Vec<usize> = (0..b.len())
                    .filter(|&j| first.is_some_and(|f| b[j].keys.contains(&f)))
                    .collect();
                let cycle = match same.iter().position(|&j| j == i) {
                    Some(pos) => devices::cycle_label(pos, same.len()),
                    None => None,
                }
                .map(|(n, m)| {
                    fmt(
                        t(self.lang, Key::Cycle),
                        &[("i", &n.to_string()), ("n", &m.to_string())],
                    )
                })
                .unwrap_or_default();
                let typed = r.keys.iter().find(|k| hotkey::typed_key_warning(k));
                let note = match (self.row_err, typed) {
                    (Some((j, k)), _) if j == i => t(self.lang, k).to_string(),
                    (_, Some(k)) => fmt(
                        t(self.lang, Key::TypedKeyWarn),
                        &[("key", &hotkey::label(k))],
                    ),
                    _ => String::new(),
                };
                let keys: Vec<KeyChip> = r
                    .keys
                    .iter()
                    .enumerate()
                    .map(|(j, k)| KeyChip {
                        label: hotkey::label(k).into(),
                        recording: self.recording == Some((i, Some(j))),
                    })
                    .collect();
                BindRow {
                    keys: model(keys),
                    adding: self.recording == Some((i, None)),
                    device: index_of(&r.device, &self.out_opts),
                    cycle: cycle.into(),
                    note: note.into(),
                }
            })
            .collect();
        w.set_binds(model(rows));
    }

    fn render_guard(&self, w: &SettingsView) {
        let same = Opt {
            label: t(self.lang, Key::SameAsAbove).into(),
            enabled: true,
        };
        let none = Opt {
            label: t(self.lang, Key::GuardNone).into(),
            enabled: true,
        };
        let first =
            |o: Opt, v: &[Opt]| model(std::iter::once(o).chain(v.iter().cloned()).collect());
        let (outs, ins) = (self.opts(&self.out_opts), self.opts(&self.in_opts));
        w.set_out_devices(model(outs.clone()));
        w.set_in_devices(model(ins.clone()));
        w.set_out_guard_devices(first(none.clone(), &outs));
        w.set_in_guard_devices(first(none, &ins));
        w.set_out_comms(first(same.clone(), &outs));
        w.set_in_comms(first(same, &ins));
        let side = |g: &GuardSide, opts: &[DeviceInfo]| {
            (
                g.default.as_ref().map_or(0, |r| index_of(r, opts) + 1),
                g.comms.as_ref().map_or(0, |r| index_of(r, opts) + 1),
                GUARD_MODES.iter().position(|m| *m == g.mode).unwrap_or(0) as i32,
            )
        };
        let (m, c, md) = side(&self.cfg.guard_out, &self.out_opts);
        w.set_out_main(m);
        w.set_out_comm(c);
        w.set_out_mode(md);
        let (m, c, md) = side(&self.cfg.guard_in, &self.in_opts);
        w.set_in_main(m);
        w.set_in_comm(c);
        w.set_in_mode(md);
        w.set_guard_enabled(self.cfg.guard_enabled);
        w.set_snooze(self.cfg.snooze_min as i32);
    }

    fn render_general(&self, w: &SettingsView) {
        let l = self.lang;
        w.set_lang(
            LANG_CHOICES
                .iter()
                .position(|x| *x == self.cfg.lang)
                .unwrap_or(0) as i32,
        );
        w.set_autostart(self.cfg.autostart);
        w.set_admin(self.cfg.run_as_admin);
        w.set_admin_available(self.admin_ok && !self.admin_busy);
        let sub = if !self.admin_ok {
            Key::AdminNeedsInstall
        } else if self.admin_failed {
            Key::AdminFailed
        } else {
            Key::RunAsAdminSub
        };
        w.set_admin_sub(t(l, sub).into());
        w.set_version(APP_VERSION.into());
        let status = match &self.upd {
            Upd::Idle => String::new(),
            Upd::Checking => t(l, Key::UpdateChecking).into(),
            Upd::Latest => t(l, Key::UpdateLatest).into(),
            Upd::Available(r) => fmt(
                t(l, Key::UpdateAvailable),
                &[("ver", r.tag.trim_start_matches('v'))],
            ),
            Upd::Failed => t(l, Key::UpdateFailed).into(),
            Upd::Downloading => t(l, Key::UpdateDownloading).into(),
            Upd::InstallFailed => t(l, Key::UpdateInstallFailed).into(),
        };
        w.set_update_line(status.into());
        let button = match self.upd {
            Upd::Available(_) => Key::InstallUpdate,
            _ => Key::CheckUpdate,
        };
        w.set_update_button(t(l, button).into());
        w.set_update_running(matches!(self.upd, Upd::Checking | Upd::Downloading));
    }

    fn render_all(&self, w: &SettingsView) {
        apply_strings(w, self.lang);
        w.set_osd(self.cfg.osd);
        w.set_also_comms(self.cfg.also_comms);
        self.render_binds(w);
        self.render_names(w);
        self.render_guard(w);
        self.render_general(w);
        w.set_error(self.error.map_or("", |k| t(self.lang, k)).into());
    }

    fn changed(&self) {
        self.save_timer
            .start(TimerMode::SingleShot, Duration::from_millis(300), || {
                with(|st, w| {
                    st.save_now(w);
                });
            });
    }

    /// True when the edited config reached disk.
    fn save_now(&mut self, w: &SettingsView) -> bool {
        self.save_timer.stop();
        let Some(path) = self.path.clone() else {
            return false;
        };
        // Only a parsed file holds core changes worth keeping; defaults would undo the user's edits.
        let merged = match (self.blocked, config::load(&path)) {
            (false, LoadOutcome::Loaded(disk)) => {
                config::merge_for_save(&disk, &self.cfg, self.touched)
            }
            (false, LoadOutcome::Fresh(_) | LoadOutcome::Corrupt(_)) => self.cfg.clone(),
            _ => {
                self.set_error(w, Some(Key::SaveBlocked));
                return false;
            }
        };
        if let Err(e) = config::save(&path, &merged) {
            log::error(&format!("settings: save failed: {e}"));
            self.set_error(w, Some(Key::SaveFailed));
            return false;
        }
        self.set_error(w, error_after_save(self.error));
        // The core may have moved guard targets meanwhile; show what was kept.
        if merged != self.cfg {
            self.cfg = merged;
            self.render_guard(w);
        }
        // SAFETY: static class name; PostMessageW only queues and a missing core is fine.
        unsafe {
            if let Ok(h) = FindWindowW(app::CORE_CLASS, PCWSTR::null()) {
                let _ = PostMessageW(Some(h), app::WM_APP_RELOAD, WPARAM(0), LPARAM(0));
            }
        }
        true
    }

    fn set_error(&mut self, w: &SettingsView, e: Option<Key>) {
        self.error = e;
        w.set_error(e.map_or("", |k| t(self.lang, k)).into());
    }

    fn stop_recording(&mut self) {
        if self.recording.take().is_some() {
            record::stop();
        }
    }

    /// Another row already holds this exact key and device.
    fn duplicate(&self, i: usize, key: Hotkey, dev: &str) -> bool {
        key.vk != 0
            && self
                .cfg
                .bindings
                .iter()
                .enumerate()
                .any(|(j, b)| j != i && b.keys.contains(&key) && b.device.id == dev)
    }

    fn on_sink(&mut self, w: &SettingsView, ev: SinkEvent) {
        match ev {
            SinkEvent::Devices => {
                self.refresh_devices();
                self.render_binds(w);
                self.render_guard(w);
            }
            SinkEvent::Cancel => {
                self.stop_recording();
                self.render_binds(w);
            }
            SinkEvent::Key(key) => {
                let Some((i, slot)) = self.recording else {
                    return;
                };
                self.stop_recording();
                let Some(b) = self.cfg.bindings.get(i) else {
                    return;
                };
                let dev = b.device.id.clone();
                let already = b.keys.contains(&key);
                self.row_err = if hotkey::check(&key).is_err() {
                    Some((i, Key::KeyInvalid))
                } else if already {
                    // The row already answers to this key.
                    None
                } else if self.duplicate(i, key, &dev) {
                    Some((i, Key::DuplicatePair))
                } else {
                    let keys = &mut self.cfg.bindings[i].keys;
                    match slot.filter(|&j| j < keys.len()) {
                        Some(j) => keys[j] = key,
                        None => keys.push(key),
                    }
                    self.changed();
                    None
                };
                self.render_binds(w);
            }
        }
    }

    fn start_check(&mut self, w: &SettingsView) {
        self.upd = Upd::Checking;
        self.cfg.last_update_check = now_secs();
        self.changed();
        self.render_general(w);
        std::thread::spawn(|| {
            let r = update::latest_release();
            let _ = slint::invoke_from_event_loop(move || {
                with(|st, w| {
                    st.upd = match r {
                        Ok(rel) if update::newer_than(&rel.tag, APP_VERSION) => {
                            if update::pick_asset(&rel).is_some() {
                                Upd::Available(rel)
                            } else {
                                log::line("update: newer release without a usable asset");
                                Upd::Failed
                            }
                        }
                        Ok(_) => Upd::Latest,
                        Err(e) => {
                            log::line(&format!("update: check failed: {e}"));
                            Upd::Failed
                        }
                    };
                    st.render_general(w);
                });
            });
        });
    }

    fn start_install(&mut self, w: &SettingsView) {
        let Upd::Available(rel) = &self.upd else {
            return;
        };
        let Some(asset) = update::pick_asset(rel).cloned() else {
            return;
        };
        self.upd = Upd::Downloading;
        self.render_general(w);
        std::thread::spawn(move || {
            let r = update::download_installer(&asset, &mut |_| {}).and_then(update::run_silent);
            let _ = slint::invoke_from_event_loop(move || match r {
                // The installer replaces this exe; step out of its way.
                Ok(()) => {
                    let _ = slint::quit_event_loop();
                }
                Err(e) => {
                    log::error(&format!("update: install failed: {e}"));
                    with(|st, w| {
                        st.upd = Upd::InstallFailed;
                        st.render_general(w);
                    });
                }
            });
        });
    }

    /// Saves the new mode first so a restarted core already sees it; reverts if the save
    /// fails or the elevated helper is refused or fails. UAC blocks, so it runs off the UI thread.
    fn set_admin(&mut self, w: &SettingsView, on: bool) {
        if !self.admin_ok || self.admin_busy {
            return;
        }
        let before = self.cfg.run_as_admin;
        self.cfg.run_as_admin = on;
        self.admin_failed = false;
        if !self.save_now(w) {
            self.cfg.run_as_admin = before;
            self.render_general(w);
            return;
        }
        self.admin_busy = true;
        // SAFETY: static class name; a null title matches any window of that class.
        let owner =
            unsafe { FindWindowW(WINDOW_CLASS, PCWSTR::null()) }.map_or(0, |h| h.0 as isize);
        std::thread::spawn(move || {
            // SAFETY: once on this new thread, before ShellExecuteExW uses COM.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            let r = launch_elevated(on, owner).map(exit_code_of);
            let _ = slint::invoke_from_event_loop(move || {
                with(|st, w| {
                    st.admin_busy = false;
                    let ok = match r {
                        Ok(0) => true,
                        Ok(code) => {
                            log::line(&format!("settings: admin helper exit {code}"));
                            st.admin_failed = true;
                            false
                        }
                        Err(e) => {
                            log::error(&format!("settings: admin helper not started: {e}"));
                            false
                        }
                    };
                    if !ok {
                        st.cfg.run_as_admin = before;
                        st.save_now(w);
                    }
                    st.render_general(w);
                });
            });
        });
        self.render_general(w);
    }
}

/// Launches `swara.exe --admin-on` or `--admin-off` through UAC, with `owner` as the prompt's
/// parent. The process handle comes back as an integer so it can be sent to another thread.
fn launch_elevated(on: bool, owner: isize) -> Result<isize, String> {
    let flag = if on { "--admin-on" } else { "--admin-off" };
    let self_path = std::env::current_exe().map_err(|e| format!("own path: {e}"))?;
    let program = HSTRING::from(self_path.as_os_str());
    let args = HSTRING::from(flag);
    let mut request = SHELLEXECUTEINFOW {
        cbSize: EXEC_INFO_BYTES,
        hwnd: HWND(owner as *mut _),
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR::from_raw(program.as_ptr()),
        lpParameters: PCWSTR::from_raw(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `program` and `args` outlive the call; `exit_code_of` closes the returned handle.
    unsafe { ShellExecuteExW(&mut request) }.map_err(|e| format!("runas: {e}"))?;
    match request.hProcess {
        child if child.is_invalid() => Err("runas returned no process".into()),
        child => Ok(child.0 as isize),
    }
}

/// Blocks until the helper exits; 1 when its exit code cannot be read.
fn exit_code_of(raw: isize) -> u32 {
    let child = HANDLE(raw as *mut _);
    let mut code = 0u32;
    // SAFETY: `child` is the handle from ShellExecuteExW; this is its only owner and closes it.
    let read = unsafe {
        let _ = WaitForSingleObject(child, INFINITE);
        let read = GetExitCodeProcess(child, &mut code).is_ok();
        let _ = CloseHandle(child);
        read
    };
    if read { code } else { 1 }
}

/// The corrupt-config notice must outlive the first save, which may follow within a second.
fn error_after_save(e: Option<Key>) -> Option<Key> {
    e.filter(|&k| k == Key::ConfigCorrupt)
}

fn open_url(url: &str) {
    // An elevated settings window would otherwise start the browser elevated too.
    if admin::running_elevated() {
        admin::open_unelevated(std::path::Path::new(url));
        return;
    }
    let u = HSTRING::from(url);
    // SAFETY: both strings outlive the call; the result is only checked for failure.
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(u.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize <= 32 {
        log::error(&format!("settings: open link failed: {}", r.0 as isize));
    }
}

fn admin_allowed() -> bool {
    admin::account_is_admin()
        && std::env::current_exe()
            .is_ok_and(|e| admin::under_program_files(&e, &admin::program_files_roots()))
}

fn wire(w: &SettingsView) {
    w.on_record(|i, j| {
        with(|st, w| {
            // -1 is the "+" / "Set key" box: record a key to add.
            let target = (i as usize, usize::try_from(j).ok());
            let again = st.recording == Some(target);
            st.stop_recording();
            st.row_err = None;
            if !again {
                match record::start(st.sink) {
                    Ok(()) => st.recording = Some(target),
                    Err(e) => log::error(&format!("settings: raw input failed: {e}")),
                }
            }
            st.render_binds(w);
        });
    });
    w.on_remove_key(|i, j| {
        with(|st, w| {
            let (i, j) = (i as usize, j as usize);
            if let Some(b) = st.cfg.bindings.get_mut(i)
                && b.keys.len() > 1
                && j < b.keys.len()
            {
                b.keys.remove(j);
                st.stop_recording();
                st.row_err = None;
                st.changed();
                st.render_binds(w);
            }
        });
    });
    w.on_pick_device(|i, d| {
        with(|st, w| {
            let (i, d) = (i as usize, d as usize);
            let (Some(dev), Some(b)) = (st.out_opts.get(d), st.cfg.bindings.get(i)) else {
                return;
            };
            if b.keys.iter().any(|&k| st.duplicate(i, k, &dev.id)) {
                st.row_err = Some((i, Key::DuplicatePair));
            } else {
                st.cfg.bindings[i].device = dev_ref(dev);
                st.row_err = None;
                st.changed();
            }
            st.render_binds(w);
        });
    });
    w.on_remove(|i| {
        with(|st, w| {
            let i = i as usize;
            if i < st.cfg.bindings.len() {
                st.stop_recording();
                st.row_err = None;
                st.cfg.bindings.remove(i);
                st.changed();
                st.render_binds(w);
            }
        });
    });
    w.on_add(|| {
        with(|st, w| {
            // A device no row uses yet, so recording a key already in use makes a cycle
            // instead of a duplicate pair.
            let used = |d: &DeviceInfo| st.cfg.bindings.iter().any(|b| b.device.id == d.id);
            let present = || st.out_opts.iter().filter(|d| d.present);
            let device = present()
                .find(|d| !used(d))
                .or_else(|| present().next())
                .map(dev_ref)
                .unwrap_or_default();
            st.cfg.bindings.push(Binding {
                keys: Vec::new(),
                device,
            });
            st.render_binds(w);
        });
    });
    w.on_drop_index(|i, y, h| {
        ST.with(|c| {
            let len = c.borrow().as_ref().map_or(0, |st| st.cfg.bindings.len());
            drag::reorder(len, i as usize, y, h) as i32
        })
    });
    w.on_drop(|i, y, h| {
        with(|st, w| {
            let from = i as usize;
            let len = st.cfg.bindings.len();
            if from >= len {
                return;
            }
            let to = drag::reorder(len, from, y, h);
            if to != from {
                st.stop_recording();
                st.row_err = None;
                let b = st.cfg.bindings.remove(from);
                st.cfg.bindings.insert(to, b);
                st.changed();
                st.render_binds(w);
            }
        });
    });
    w.on_set_osd(|v| {
        with(|st, w| {
            st.cfg.osd = v;
            w.set_osd(v);
            st.changed();
        });
    });
    w.on_set_also_comms(|v| {
        with(|st, w| {
            st.cfg.also_comms = v;
            w.set_also_comms(v);
            st.changed();
        });
    });
    w.on_nick_edited(|i, s| {
        with(|st, w| {
            let Some(id) = st.name_ids.get(i as usize).cloned() else {
                return;
            };
            let nick = s.trim();
            // An empty nickname would show a blank name, so clearing removes the key.
            if nick.is_empty() {
                st.cfg.nicknames.remove(&id);
            } else {
                st.cfg.nicknames.insert(id, nick.to_string());
            }
            st.changed();
            st.render_guard(w);
        });
    });
    w.on_set_guard_enabled(|v| {
        with(|st, w| {
            st.cfg.guard_enabled = v;
            st.touched.guard_enabled = true;
            w.set_guard_enabled(v);
            st.changed();
        });
    });
    w.on_guard_main(|out, i| {
        with(|st, w| {
            let opts = if out { &st.out_opts } else { &st.in_opts };
            // Index 0 is "None": stop guarding that side's device.
            let dev = match i {
                0 => None,
                n => match opts.get(n as usize - 1) {
                    Some(d) => Some(dev_ref(d)),
                    None => return,
                },
            };
            let side = if out {
                &mut st.cfg.guard_out
            } else {
                &mut st.cfg.guard_in
            };
            // Picking a device to guard means the user wants guarding on that side.
            if dev.is_some() && side.default.is_none() && side.mode == GuardMode::Off {
                side.mode = GuardMode::Notify;
            }
            side.default = dev;
            if out {
                st.touched.guard_out = true;
            } else {
                st.touched.guard_in = true;
            }
            st.changed();
            st.render_guard(w);
        });
    });
    w.on_guard_comms(|out, i| {
        with(|st, w| {
            let opts = if out { &st.out_opts } else { &st.in_opts };
            let comms = match i {
                0 => None,
                n => match opts.get(n as usize - 1) {
                    Some(d) => Some(dev_ref(d)),
                    None => return,
                },
            };
            if out {
                st.cfg.guard_out.comms = comms;
            } else {
                st.cfg.guard_in.comms = comms;
            }
            if out {
                st.touched.guard_out = true;
            } else {
                st.touched.guard_in = true;
            }
            st.changed();
            st.render_guard(w);
        });
    });
    w.on_guard_mode(|out, i| {
        with(|st, w| {
            let mode = GUARD_MODES.get(i as usize).copied().unwrap_or_default();
            if out {
                st.cfg.guard_out.mode = mode;
            } else {
                st.cfg.guard_in.mode = mode;
            }
            st.changed();
            st.render_guard(w);
        });
    });
    w.on_snooze_step(|d| {
        with(|st, w| {
            st.cfg.snooze_min = st.cfg.snooze_min.saturating_add_signed(d).clamp(1, 120);
            w.set_snooze(st.cfg.snooze_min as i32);
            st.changed();
        });
    });
    w.on_set_lang(|i| {
        with(|st, w| {
            st.cfg.lang = LANG_CHOICES.get(i as usize).cloned().unwrap_or_default();
            st.lang = i18n::current(st.cfg.lang.clone());
            st.changed();
            st.render_all(w);
        });
    });
    w.on_set_autostart(|v| {
        with(|st, w| {
            st.cfg.autostart = v;
            w.set_autostart(v);
            st.changed();
        });
    });
    w.on_set_admin(|v| {
        with(|st, w| st.set_admin(w, v));
    });
    w.on_update_clicked(|| {
        with(|st, w| match st.upd {
            Upd::Available(_) => st.start_install(w),
            Upd::Checking | Upd::Downloading => {}
            _ => st.start_check(w),
        });
    });
    w.on_open_link(|i| {
        if let Some(u) = LINKS.get(i as usize) {
            open_url(u);
        }
    });
}

pub fn run() -> i32 {
    let selected = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .with_winit_window_attributes_hook(|a| {
            use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
            use slint::winit_030::winit::window::WindowButtons;
            // Fixed-size window: no maximize button.
            let a = a
                .with_class_name(WINDOW_CLASS_STR)
                .with_enabled_buttons(WindowButtons::CLOSE | WindowButtons::MINIMIZE);
            match super::dpi::centered(WINDOW_W, WINDOW_H) {
                Some((x, y)) => {
                    a.with_position(slint::winit_030::winit::dpi::PhysicalPosition::new(x, y))
                }
                None => a,
            }
        })
        .select();
    if let Err(e) = selected {
        log::error(&format!("settings: backend: {e}"));
        return 1;
    }
    let w = match SettingsView::new() {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("settings: window: {e}"));
            return 1;
        }
    };
    let sink = match record::create_sink() {
        Ok(h) => h,
        Err(e) => {
            log::error(&format!("settings: sink window: {e}"));
            return 1;
        }
    };
    let audio = Audio::new(sink)
        .inspect_err(|e| log::error(&format!("settings: audio: {e}")))
        .ok();
    let path = config::config_file();
    let (cfg, blocked, error) = match path.as_deref().map(config::load) {
        Some(LoadOutcome::Fresh(c) | LoadOutcome::Loaded(c)) => (c, false, None),
        Some(LoadOutcome::Corrupt(c)) => (c, false, Some(Key::ConfigCorrupt)),
        Some(LoadOutcome::Unreadable(c)) => (c, true, Some(Key::SaveBlocked)),
        None => (Config::default(), true, Some(Key::SaveBlocked)),
    };
    let mut st = State {
        w: w.as_weak(),
        lang: i18n::current(cfg.lang.clone()),
        cfg,
        path,
        blocked,
        touched: config::Touched::default(),
        audio,
        sink,
        out_opts: Vec::new(),
        in_opts: Vec::new(),
        name_ids: Vec::new(),
        recording: None,
        row_err: None,
        error,
        upd: Upd::Idle,
        admin_ok: admin_allowed(),
        admin_busy: false,
        admin_failed: false,
        save_timer: Timer::default(),
    };
    st.refresh_devices();
    w.set_win_title("Swara".into());
    st.render_all(&w);
    let due = now_secs().saturating_sub(st.cfg.last_update_check) > DAY_SECS;
    ST.with(|c| *c.borrow_mut() = Some(st));
    wire(&w);
    let handle = w.as_weak();
    let mut subclassed = false;
    w.window().on_winit_window_event(move |window, event| {
        if !std::mem::replace(&mut subclassed, true)
            && window.with_winit_window(super::dpi::install) != Some(true)
        {
            log::error("settings dpi hook not installed");
        }
        match event {
            // Losing focus ends recording, or keys meant for another app would land in the sink.
            WindowEvent::Focused(false) => {
                let _ = slint::invoke_from_event_loop(cancel_recording);
            }
            // Give Slint a moment to finish rescaling, then restore the fixed logical size.
            WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Resized(_) => {
                let later = handle.clone();
                Timer::single_shot(RESIZE_SETTLE, move || {
                    if let Some(view) = later.upgrade() {
                        keep_fixed_size(view.window());
                    }
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });
    record::set_handler(|ev| {
        with(|st, w| st.on_sink(w, ev));
    });
    if due && !blocked {
        with(|st, w| st.start_check(w));
    }
    let r = w.run();
    with(|st, w| {
        st.stop_recording();
        if st.save_timer.running() {
            st.save_now(w);
        }
    });
    ST.with(|c| c.borrow_mut().take());
    match r {
        Ok(()) => 0,
        Err(e) => {
            log::error(&format!("settings: event loop: {e}"));
            1
        }
    }
}

fn cancel_recording() {
    with(|st, w| {
        if st.recording.is_some() {
            st.on_sink(w, SinkEvent::Cancel);
        }
    });
}

/// Off by more than a pixel either way means the size drifted, so put it back.
fn keep_fixed_size(window: &slint::Window) {
    let fixed = slint::LogicalSize::new(WINDOW_W, WINDOW_H);
    let target = fixed.to_physical(window.scale_factor());
    let actual = window.size();
    let drifted = |a: u32, b: u32| a.abs_diff(b) > 1;
    if drifted(actual.width, target.width) || drifted(actual.height, target.height) {
        window.set_size(fixed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_keeps_corrupt_notice_only() {
        let k = Some(Key::ConfigCorrupt);
        assert_eq!(error_after_save(k), k);
        assert_eq!(error_after_save(Some(Key::SaveFailed)), None);
        assert_eq!(error_after_save(None), None);
    }
}
