//! Anonymous usage stats: install, update, a daily ping, features in use, and errors.
//! Release builds only. Nothing sent names the user, their devices, keys, or settings.

use crate::config::{self, Config, GuardMode, GuardSide};
use crate::i18n::ResolvedLang;
use crate::log;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const ENABLED: bool = !cfg!(debug_assertions);
const STATS_HOST: &str = "dash.kurohiko.id";
const APP: &str = "swara";
const TIMEOUT_MS: i32 = 8000;
const STATE_FILE: &str = "stats.json";
const INPUT_CAP: usize = 20_000;
const ERROR_BUDGET: usize = 10;
/// After a failed ping, later wakes leave it alone for this long.
const PING_BACKOFF: Duration = Duration::from_secs(60 * 60);

// Hand-written twin of the dashboard's path filter (case-insensitive, global):
//   (?:\b[a-z][\w+.-]{0,30}:\/{0,3}(?:[a-z]:)?|\b[a-z]:|(?<![\w.-]))(?:[\\/][^\\/\n"<>|?*:]*)*[\\/]
// The regex crate cannot do the lookbehind, and a single scan stays linear on huge input.
// Directories go; the file name and position survive.

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

fn is_seg(c: char) -> bool {
    !matches!(
        c,
        '\\' | '/' | '\n' | '"' | '<' | '>' | '|' | '?' | '*' | ':'
    )
}

/// Requires a separator at `p`; returns the index right after the final separator that
/// comes before the first character a folder name cannot contain.
fn path_end(s: &[char], p: usize) -> Option<usize> {
    if !s.get(p).is_some_and(|&c| is_sep(c)) {
        return None;
    }
    let mut end = p + 1;
    for (i, &c) in s.iter().enumerate().skip(p + 1) {
        if is_sep(c) {
            end = i + 1;
        } else if !is_seg(c) {
            break;
        }
    }
    Some(end)
}

fn is_drive(s: &[char], q: usize) -> bool {
    s.get(q).is_some_and(|c| c.is_ascii_alphabetic()) && s.get(q + 1) == Some(&':')
}

/// Where a path match beginning at `i` ends, testing the alternatives in pattern order.
fn match_at(s: &[char], i: usize) -> Option<usize> {
    let prev = i.checked_sub(1).map(|j| s[j]);
    if s[i].is_ascii_alphabetic() && !prev.is_some_and(is_word) {
        // A URL scheme plus up to three slashes and an optional drive; `C:` alone counts
        // as a one-letter scheme.
        let mut j = i + 1;
        while j < s.len() && j - i <= 30 && (is_word(s[j]) || matches!(s[j], '+' | '.' | '-')) {
            j += 1;
        }
        if s.get(j) == Some(&':') {
            let start = j + 1;
            let n = s[start..].iter().take(3).take_while(|&&c| c == '/').count();
            for q in (start..=start + n).rev() {
                if is_drive(s, q)
                    && let Some(e) = path_end(s, q + 2)
                {
                    return Some(e);
                }
                if let Some(e) = path_end(s, q) {
                    return Some(e);
                }
            }
        }
    }
    if prev.is_some_and(|c| is_word(c) || c == '.' || c == '-') {
        return None;
    }
    path_end(s, i)
}

/// Removes every directory part so only file names and line numbers remain. Characters
/// beyond `INPUT_CAP` are discarded before scanning.
pub fn drop_folders(text: &str) -> String {
    let chars: Vec<char> = text.chars().take(INPUT_CAP).collect();
    let mut kept = String::new();
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        if let Some(next) = match_at(&chars, at) {
            at = next;
        } else {
            kept.push(c);
            at += 1;
        }
    }
    kept
}

fn anonymize(text: &str, user: &str) -> String {
    let cleaned = drop_folders(text);
    match user {
        "" => cleaned,
        name => cleaned.replace(name, "<user>"),
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn error_props(msg: &str, stack: &str, user: &str) -> Value {
    let message = match msg {
        "" => String::from("Error"),
        text => anonymize(text, user),
    };
    let cleaned_stack = anonymize(stack, user);
    let frames: Vec<&str> = cleaned_stack.split('\n').skip(1).take(3).collect();
    json!({
        "msg": clip(&message, 300),
        "stack": clip(&frames.join("\n"), 600),
    })
}

/// Each distinct message goes out once per session, and never more than `ERROR_BUDGET`.
#[derive(Default)]
struct ErrorGate(HashSet<String>);

impl ErrorGate {
    fn admit(&mut self, msg: &str) -> bool {
        if self.0.len() >= ERROR_BUDGET {
            return false;
        }
        self.0.insert(msg.to_owned())
    }
}

fn os_name(major: u32, minor: u32, build: u32) -> String {
    match (major, build) {
        (_, 22000..) => "Windows 11".into(),
        (10, _) => "Windows 10".into(),
        _ => format!("Windows {major}.{minor}.{build}"),
    }
}

fn arch_name(rust: &str) -> &str {
    match rust {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

fn windows_version() -> (u32, u32, u32) {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::{PCWSTR, w};
    const NT_KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let read_dword = |value: PCWSTR| -> u32 {
        let mut number = 0u32;
        let mut size = std::mem::size_of_val(&number) as u32;
        // SAFETY: static value names; `number` lives past the call and `size` is its size.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                NT_KEY,
                value,
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut number).cast()),
                Some(&mut size),
            )
        };
        if status.is_ok() { number } else { 0 }
    };
    let mut digits = [0u16; 16];
    let mut size = std::mem::size_of_val(&digits) as u32;
    // SAFETY: static value names; `digits` lives past the call and `size` is its size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            NT_KEY,
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(digits.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    let build = match status.is_ok() {
        true => {
            let units = digits.len().min(size as usize / 2);
            let text = String::from_utf16_lossy(&digits[..units]);
            text.trim_end_matches('\0').parse().unwrap_or(0)
        }
        false => 0,
    };
    let major = read_dword(w!("CurrentMajorVersionNumber"));
    let minor = read_dword(w!("CurrentMinorVersionNumber"));
    (major, minor, build)
}

fn locale() -> String {
    let mut name = [0u16; 85];
    // SAFETY: `name` holds LOCALE_NAME_MAX_LENGTH writable UTF-16 units.
    let written = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut name) };
    // The count includes the trailing NUL, and 0 signals failure.
    let units = usize::try_from(written)
        .unwrap_or(0)
        .saturating_sub(1)
        .min(name.len());
    clip(&String::from_utf16_lossy(&name[..units]), 16)
}

fn today_utc() -> String {
    // SAFETY: takes nothing and returns the time by value.
    let now = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

fn format_uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = 0x40 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn fresh_uid() -> Option<String> {
    use windows::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut random = [0u8; 16];
    // SAFETY: `random` is writable; the system-RNG flag means no algorithm handle is needed.
    let status = unsafe { BCryptGenRandom(None, &mut random, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status.is_ok().then(|| format_uuid_v4(random))
}

/// Same rule the server applies: `^[A-Za-z0-9-]{8,64}$`.
fn uid_ok(uid: &str) -> bool {
    let sized = (8..=64).contains(&uid.len());
    sized && uid.chars().all(|c| c == '-' || c.is_ascii_alphanumeric())
}

/// Features switched on in `cfg`. Fixed names only; nothing from the config itself.
pub fn features(cfg: &Config, lang: ResolvedLang) -> Vec<&'static str> {
    let guarded = |s: &GuardSide| {
        cfg.guard_enabled && s.mode != GuardMode::Off && (s.default.is_some() || s.comms.is_some())
    };
    [
        (guarded(&cfg.guard_out), "f_guard_out"),
        (guarded(&cfg.guard_in), "f_guard_in"),
        (cfg.autostart, "f_autostart"),
        (cfg.run_as_admin, "f_admin"),
        (cfg.osd, "f_osd"),
        (cfg.also_comms, "f_also_comms"),
        (!cfg.nicknames.is_empty(), "f_nick"),
        (lang == ResolvedLang::Id, "f_lang_id"),
        (lang == ResolvedLang::En, "f_lang_en"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect()
}

fn base(uid: &str) -> Value {
    let (major, minor, build) = windows_version();
    json!({
        "app": APP,
        "uid": uid,
        "ver": env!("CARGO_PKG_VERSION"),
        "os": os_name(major, minor, build),
        "arch": arch_name(std::env::consts::ARCH),
        "lang": locale(),
    })
}

fn body(base: &Value, ev: &str, props: Option<Value>) -> Value {
    let mut b = base.clone();
    b["ev"] = ev.into();
    if let Some(p) = props {
        b["props"] = p;
    }
    b
}

/// Contents of `stats.json`.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
struct Saved {
    #[serde(default)]
    uid: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    installed: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ver: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    day: String,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// First run sends `install`; a version change sends `update`. If `update` fails the stored
/// version stays old, so the next start tries again.
fn on_start(st: &mut Saved, ver: &str, send: &mut dyn FnMut(&str, Option<Value>) -> bool) {
    let mut ok = true;
    if st.installed == 0 {
        if send("install", None) {
            st.installed = 1;
        }
    } else if !st.ver.is_empty() && st.ver != ver {
        ok = send("update", Some(json!({ "from": st.ver })));
    }
    if ok {
        st.ver = ver.to_string();
    }
}

enum Job {
    Wake(Vec<&'static str>),
    Error(String),
    Flush(Sender<()>),
}

struct Worker {
    st: Saved,
    /// `None` in the settings process, which only reports errors and never writes.
    path: Option<PathBuf>,
    base: Value,
    user: String,
    ping_after: Option<Instant>,
    feat_day: String,
    feats: HashSet<&'static str>,
    errors: ErrorGate,
}

impl Worker {
    fn send(&self, ev: &str, props: Option<Value>) -> bool {
        let payload = body(&self.base, ev, props).to_string();
        match crate::http::post_json(STATS_HOST, "/e", payload.as_bytes(), TIMEOUT_MS) {
            Ok(code) if (200..300).contains(&code) => true,
            Ok(code) => {
                log::line(&format!("stats {ev} rejected with HTTP {code}"));
                false
            }
            Err(e) => {
                log::line(&format!("stats {ev} not sent: {e}"));
                false
            }
        }
    }

    fn save(&self) {
        if let Some(p) = &self.path
            && let Ok(b) = serde_json::to_vec(&self.st)
            && let Err(e) = config::write_atomic(p, &b)
        {
            log::line(&format!("stats state not saved: {e}"));
        }
    }

    fn start(&mut self) {
        let mut st = self.st.clone();
        on_start(&mut st, env!("CARGO_PKG_VERSION"), &mut |ev, p| {
            self.send(ev, p)
        });
        self.st = st;
        self.save();
    }

    fn wake(&mut self, feats: &[&'static str]) {
        let today = today_utc();
        if self.st.day != today && self.ping_after.is_none_or(|t| Instant::now() >= t) {
            if self.send("ping", None) {
                self.st.day = today.clone();
                self.ping_after = None;
                self.save();
            } else {
                self.ping_after = Some(Instant::now() + PING_BACKOFF);
            }
        }
        if self.feat_day != today {
            self.feat_day = today;
            self.feats.clear();
        }
        for &f in feats {
            // Recorded before the send result is known, so a failure is not retried today.
            if self.feats.insert(f) {
                self.send(f, None);
            }
        }
    }

    fn error(&mut self, msg: &str) {
        let props = error_props(msg, "", &self.user);
        if self.errors.admit(props["msg"].as_str().unwrap_or_default()) {
            self.send("error", Some(props));
        }
    }

    fn run(mut self, rx: Receiver<Job>, core: bool) {
        if core {
            self.start();
        }
        while let Ok(job) = rx.recv() {
            match job {
                Job::Wake(f) if core => self.wake(&f),
                Job::Wake(_) => {}
                Job::Error(m) => self.error(&m),
                Job::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }
}

static QUEUE: OnceLock<Sender<Job>> = OnceLock::new();

/// Core process: install/update now, then pings, features, and errors as they come.
pub fn init_core() {
    spawn(true);
}

/// Settings process: errors only, under the ID the core already created.
pub fn init_errors_only() {
    spawn(false);
}

fn spawn(core: bool) {
    if !ENABLED {
        return;
    }
    let Some(path) = config::app_dir().map(|d| d.join(STATE_FILE)) else {
        return;
    };
    let read = config::read_capped(&path);
    // Only a missing file earns a new ID; any other read error would split one install in two.
    if read
        .as_ref()
        .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
    {
        return;
    }
    let mut st: Saved = read
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();
    if !uid_ok(&st.uid) {
        match fresh_uid().filter(|_| core) {
            Some(u) => {
                st = Saved {
                    uid: u,
                    ..Default::default()
                }
            }
            None => return,
        }
    }
    let (sender, inbox) = mpsc::channel();
    if QUEUE.set(sender).is_err() {
        return;
    }
    let user = std::env::var_os("USERNAME")
        .and_then(|name| name.into_string().ok())
        .unwrap_or_default();
    let worker = Worker {
        base: base(&st.uid),
        st,
        path: core.then_some(path),
        user,
        ping_after: None,
        feat_day: String::new(),
        feats: HashSet::new(),
        errors: ErrorGate::default(),
    };
    if let Err(e) = std::thread::Builder::new()
        .name("stats".into())
        .spawn(move || worker.run(inbox, core))
    {
        log::line(&format!("stats worker did not start: {e}"));
    }
}

/// Checks the daily ping and today's features. Called on events, never on a timer.
pub fn wake(cfg: &Config, lang: ResolvedLang) {
    if let Some(tx) = QUEUE.get() {
        let _ = tx.send(Job::Wake(features(cfg, lang)));
    }
}

pub fn error(msg: &str) {
    if let Some(tx) = QUEUE.get() {
        let _ = tx.send(Job::Error(msg.to_string()));
    }
}

/// Waits up to `timeout` for queued sends, for exits right after an error.
pub fn flush(timeout: Duration) {
    if let Some(tx) = QUEUE.get() {
        let (done, wait) = mpsc::channel();
        if tx.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Binding, DeviceRef, Hotkey};

    #[test]
    fn folders_are_removed_file_names_kept() {
        let table = [
            // URL scheme, drive letter, percent-encoded space.
            (
                "at load (file:///D:/Projects/My%20Tool/build/out/bundle.js:88:12)",
                "at load (bundle.js:88:12)",
            ),
            // Spaces and brackets inside a Windows folder name.
            (
                "at init (E:\\Games\\Steam Library (old)\\swara\\core.rs:41:7)",
                "at init (core.rs:41:7)",
            ),
            // UNC share.
            (
                "see \\\\fileserver\\team share\\logs\\swara.log",
                "see swara.log",
            ),
            // Unix path.
            (
                "panic at /usr/local/src/swara/main.rs:12:9",
                "panic at main.rs:12:9",
            ),
            // Apostrophe in a folder name.
            (
                "cannot open 'C:\\Users\\Dee D'Souza\\notes.txt'",
                "cannot open 'notes.txt'",
            ),
            // file URL with brackets in the user folder.
            (
                "at run (file:///C:/Users/Rina%20(home)/swara/app/main.js:7:2)",
                "at run (main.js:7:2)",
            ),
            (
                "at x (C:\\Program Files (x86)\\Swara Tools\\bin\\helper.js:3:3)",
                "at x (helper.js:3:3)",
            ),
            // A fraction before a real path: only the path goes.
            (
                "copy 4/9 failed at D:\\Rekaman\\clip.wav",
                "copy 4/9 failed at clip.wav",
            ),
            ("step 1/2 skipped", "step 1/2 skipped"),
            ("socket read/write timeout", "socket read/write timeout"),
            (
                "at emit node:events/handlers:517:28",
                "at emit node:events/handlers:517:28",
            ),
            (
                "RangeError: Invalid array length (reading 'items')",
                "RangeError: Invalid array length (reading 'items')",
            ),
        ];
        for (raw, expected) in table {
            assert_eq!(drop_folders(raw), expected, "input: {raw}");
        }
    }

    #[test]
    fn oversized_input_is_capped_and_linear() {
        let flood = "q\\".repeat(6_000_000);
        let started = Instant::now();
        let result = drop_folders(&flood);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(500), "took {took:?}");
        assert_eq!(result.chars().count(), INPUT_CAP);
    }

    #[test]
    fn error_props_are_scrubbed_and_clipped() {
        let blank = error_props("", "", "rina");
        assert_eq!(blank, json!({ "msg": "Error", "stack": "" }));

        let named = error_props(
            "config read failed for rina in C:\\Users\\rina\\AppData\\Roaming\\Swara\\config.json",
            "",
            "rina",
        );
        assert_eq!(named["msg"], "config read failed for <user> in config.json");

        let long = error_props(&"z".repeat(512), "", "");
        assert_eq!(long["msg"].as_str().unwrap().chars().count(), 300);

        let trace =
            "Error: denied\n    at open (D:\\src\\fs.rs:5)\n    at main\n    at start\n    at exit";
        let framed = error_props("denied", trace, "");
        assert_eq!(
            framed["stack"],
            "    at open (fs.rs:5)\n    at main\n    at start"
        );
        let deep = error_props("denied", &format!("head\n{}", "w".repeat(1000)), "");
        assert_eq!(deep["stack"].as_str().unwrap().chars().count(), 600);
    }

    #[test]
    fn error_gate_sends_each_message_once_up_to_budget() {
        let mut gate = ErrorGate::default();
        assert!(gate.admit("audio failed"));
        assert!(!gate.admit("audio failed"));
        for n in 2..=ERROR_BUDGET {
            assert!(gate.admit(&format!("failure #{n}")));
        }
        assert!(!gate.admit("past the budget"));
    }

    #[test]
    fn platform_labels() {
        let systems = [
            ((10, 0, 26100), "Windows 11"),
            ((10, 0, 22000), "Windows 11"),
            ((10, 0, 21999), "Windows 10"),
            ((6, 1, 7601), "Windows 6.1.7601"),
        ];
        for ((major, minor, build), want) in systems {
            assert_eq!(os_name(major, minor, build), want);
        }
        let arches = [
            ("x86_64", "x64"),
            ("aarch64", "arm64"),
            ("x86", "ia32"),
            ("riscv64", "riscv64"),
        ];
        for (rust, want) in arches {
            assert_eq!(arch_name(rust), want);
        }
    }

    #[test]
    fn uid_format_and_server_rule() {
        let zero = format_uuid_v4([0; 16]);
        assert_eq!(zero, "00000000-0000-4000-8000-000000000000");
        let full = format_uuid_v4([0xff; 16]);
        assert_eq!(full.len(), 36);
        assert_eq!(&full[14..15], "4");
        assert_eq!(&full[19..20], "b");
        assert!(uid_ok(&full));
        assert!(uid_ok("--------"));
        assert!(!uid_ok("seven77"));
        assert!(!uid_ok("contains a gap"));
        assert!(!uid_ok(&"k".repeat(65)));
    }

    fn boot(saved: &mut Saved, delivered: bool) -> Vec<String> {
        let mut events = Vec::new();
        on_start(saved, "0.4.1", &mut |ev, props| {
            let extra = props.map(|v| v.to_string()).unwrap_or_default();
            events.push(format!("{ev}{extra}"));
            delivered
        });
        events
    }

    #[test]
    fn install_then_update_events() {
        let mut saved = Saved::default();
        assert_eq!(boot(&mut saved, false), ["install"]);
        assert_eq!(saved.installed, 0);
        assert_eq!(saved.ver, "0.4.1");
        assert_eq!(boot(&mut saved, true), ["install"]);
        assert_eq!(saved.installed, 1);
        assert_eq!(boot(&mut saved, true), Vec::<String>::new());

        saved.ver = "0.3.9".into();
        let update = r#"update{"from":"0.3.9"}"#;
        assert_eq!(boot(&mut saved, false), [update]);
        assert_eq!(saved.ver, "0.3.9");
        assert_eq!(boot(&mut saved, true), [update]);
        assert_eq!(saved.ver, "0.4.1");
    }

    #[test]
    fn events_carry_no_personal_data() {
        let mic = DeviceRef {
            id: "{0.0.1.00000000}.{hidden-guid}".into(),
            name: "Studio Mic".into(),
        };
        let side = GuardSide {
            default: Some(mic.clone()),
            comms: None,
            mode: GuardMode::Notify,
        };
        let cfg = Config {
            bindings: vec![Binding {
                keys: vec![Hotkey { mods: 6, vk: 0xDC }],
                device: mic.clone(),
            }],
            nicknames: [(mic.id.clone(), "Bedroom".to_string())].into(),
            guard_enabled: true,
            guard_out: side.clone(),
            guard_in: side,
            autostart: true,
            run_as_admin: true,
            osd: true,
            also_comms: true,
            ..Config::default()
        };
        let feats = features(&cfg, ResolvedLang::Id);
        assert_eq!(feats.len(), 8);
        let base =
            json!({ "app": APP, "uid": "id", "ver": "1", "os": "w", "arch": "x", "lang": "c" });
        for ev in feats.iter().copied().chain(["install", "ping"]) {
            let name_ok = ev.len() <= 32
                && ev.starts_with(|c: char| c.is_ascii_lowercase())
                && ev
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
            assert!(name_ok, "{ev}");
            let wire = body(&base, ev, None).to_string();
            for private in ["hidden-guid", "Studio", "Bedroom", "220", "DC"] {
                assert!(!wire.contains(private), "{ev}: {wire}");
            }
        }
    }
}
