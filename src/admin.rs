//! Optional elevated mode. The normal, unelevated launch starts a per-user scheduled task that
//! runs Swara with the highest privileges. Offered only for an all-users install in Program Files.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenElevationType,
    TokenElevationTypeLimited, TokenUser,
};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::{self, Config};
use crate::{app, autostart, log};

const CORE_EXIT_WAIT_MS: u32 = 5000;

/// Fills `out` from this process's token; false when any step fails.
fn read_token(class: TOKEN_INFORMATION_CLASS, out: *mut c_void, len: u32) -> bool {
    let mut own_token = HANDLE::default();
    // SAFETY: opens our own process token for reading; closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own_token) }.is_err() {
        return false;
    }
    let mut needed = 0u32;
    // SAFETY: the caller guarantees `out` is writable for `len` bytes.
    let filled =
        unsafe { GetTokenInformation(own_token, class, Some(out), len, &mut needed) }.is_ok();
    // SAFETY: `own_token` was opened above and is not used after this.
    let _ = unsafe { CloseHandle(own_token) };
    filled
}

/// A token field of fixed size, such as `TOKEN_ELEVATION`.
fn token_value<V: Default>(kind: TOKEN_INFORMATION_CLASS) -> Option<V> {
    let mut value = V::default();
    let ptr: *mut V = &mut value;
    read_token(kind, ptr.cast(), size_of::<V>() as u32).then_some(value)
}

/// Asked once per process: elevation cannot change later, and the logger checks it per line.
pub fn running_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| {
        matches!(token_value::<TOKEN_ELEVATION>(TokenElevation), Some(e) if e.TokenIsElevated != 0)
    })
}

/// True for an administrator even when this launch is not elevated; false for a standard user.
pub fn account_is_admin() -> bool {
    // An unelevated admin holds the filtered ("limited") half of a split token.
    running_elevated()
        || token_value::<TOKEN_ELEVATION_TYPE>(TokenElevationType)
            .is_some_and(|t| t == TokenElevationTypeLimited)
}

/// Whole path parts, ignoring case, so `C:\Program Files Evil` is not treated as inside.
pub fn under_program_files(exe: &Path, roots: &[PathBuf]) -> bool {
    let exe_parts = lowered_parts(exe);
    roots
        .iter()
        .map(|r| lowered_parts(r))
        .any(|root| !root.is_empty() && exe_parts.starts_with(&root))
}

fn lowered_parts(p: &Path) -> Vec<String> {
    p.components()
        .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Taken from the shell's known folders: the environment variables are user-editable.
pub fn program_files_roots() -> Vec<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86, KF_FLAG_DEFAULT,
        SHGetKnownFolderPath,
    };
    let mut roots = Vec::new();
    for id in [
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX64,
        &FOLDERID_ProgramFilesX86,
    ] {
        // SAFETY: the shell allocates the string; it is copied and then released with
        // CoTaskMemFree, as the API requires.
        let text = unsafe {
            let Ok(raw) = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) else {
                continue;
            };
            let copied = raw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(raw.0.cast_const().cast()));
            copied
        };
        if !text.is_empty() {
            roots.push(PathBuf::from(text));
        }
    }
    roots
}

pub fn exe_under_program_files(exe: &Path) -> bool {
    under_program_files(exe, &program_files_roots())
}

pub fn elevated_task_name(account: &str) -> String {
    format!("Swara-{account}")
}

fn xml_text(raw: &str) -> String {
    const ENTITIES: [(char, &str); 5] = [
        ('&', "&amp;"),
        ('<', "&lt;"),
        ('>', "&gt;"),
        ('"', "&quot;"),
        ('\'', "&apos;"),
    ];
    raw.chars()
        .fold(String::with_capacity(raw.len()), |mut out, c| {
            match ENTITIES.iter().find(|(special, _)| *special == c) {
                Some((_, entity)) => out.push_str(entity),
                None => out.push(c),
            }
            out
        })
}

/// Has no trigger of its own; the Run-key launch at sign-in is what starts it.
pub fn scheduler_xml(exe: &Path, account: &str) -> String {
    let account = xml_text(account);
    let program = xml_text(&exe.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers />
  <Principals>
    <Principal id="Author">
      <UserId>{account}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{program}</Command>
      <Arguments>--from-task</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// `(user, domain)` read from this process's token rather than the USERNAME env vars.
fn token_account() -> Result<(String, String), String> {
    // Room for TOKEN_USER plus the largest possible SID (68 bytes); u64 keeps the pointer aligned.
    let mut raw = [0u64; 32];
    if !read_token(TokenUser, raw.as_mut_ptr().cast(), size_of_val(&raw) as u32) {
        return Err("could not read the token user".into());
    }
    // SAFETY: the TokenUser query wrote a TOKEN_USER at the start of `raw`, SID included.
    let sid = unsafe { (*raw.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut user_buf = [0u16; 257];
    let mut domain_buf = [0u16; 257];
    let mut user_len = user_buf.len() as u32;
    let mut domain_len = domain_buf.len() as u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: both buffers are writable for the lengths given; `sid` points into `raw`.
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(user_buf.as_mut_ptr())),
            &mut user_len,
            Some(PWSTR(domain_buf.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
    }
    .map_err(|e| format!("account lookup: {e}"))?;
    // After a successful call the lengths no longer count the trailing NUL.
    let user = String::from_utf16_lossy(&user_buf[..user_len as usize]);
    if user.is_empty() {
        return Err("account lookup returned no name".into());
    }
    let domain = String::from_utf16_lossy(&domain_buf[..domain_len as usize]);
    Ok((user, domain))
}

/// `DOMAIN\user`, or just `user` when the account has no domain.
fn qualified_user(domain: &str, user: &str) -> String {
    match domain {
        "" => user.to_owned(),
        d => format!("{d}\\{user}"),
    }
}

/// A full path inside a Windows folder, so an elevated process never looks a tool up on PATH.
fn in_system_folder(folder: fn(Option<&mut [u16]>) -> u32, file: &str) -> Result<PathBuf, String> {
    let mut buf = [0u16; 260];
    let len = folder(Some(&mut buf)) as usize;
    // Zero means failure; a length past the buffer is the size that would have been needed.
    if !(1..buf.len()).contains(&len) {
        return Err(format!("system folder unavailable for {file}"));
    }
    let dir = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    Ok(dir.join(file))
}

fn system_dir(buf: Option<&mut [u16]>) -> u32 {
    // SAFETY: `buf` is a valid writable slice; the binding passes its length.
    unsafe { GetSystemDirectoryW(buf) }
}

fn windows_dir(buf: Option<&mut [u16]>) -> u32 {
    // SAFETY: same contract as `system_dir`.
    unsafe { GetWindowsDirectoryW(buf) }
}

/// Each argument reaches CreateProcess as its own argv entry; no shell parses them.
fn run_schtasks(args: &[&str]) -> Result<(), String> {
    let tool = in_system_folder(system_dir, "schtasks.exe")?;
    let out = Command::new(tool)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .map_err(|e| format!("schtasks did not start: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Err(text)
    }
}

/// `create_new` refuses an existing file, so a name planted in advance cannot be reused.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("task file: {e}"))
}

pub fn install_task() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("exe path: {e}"))?;
    if !exe_under_program_files(&exe) {
        return Err("not installed under Program Files".into());
    }
    let (user, domain) = token_account()?;
    let xml = scheduler_xml(&exe, &qualified_user(&domain, &user));
    // UTF-16 LE with a BOM, matching the encoding the XML header declares.
    let encoded: Vec<u8> = std::iter::once(0xFEFF_u16)
        .chain(xml.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    // Kept next to the exe: that folder is admin-only, so nothing unelevated can swap the file
    // between this write and schtasks reading it.
    let folder = exe.parent().ok_or("exe path has no folder")?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let xml_path = folder.join(format!("swara-task-{}-{unique}.xml", std::process::id()));
    write_new(&xml_path, &encoded)?;
    let result = run_schtasks(&[
        "/Create",
        "/TN",
        &elevated_task_name(&user),
        "/XML",
        &xml_path.to_string_lossy(),
        "/F",
    ]);
    std::fs::remove_file(&xml_path).ok();
    result
}

pub fn remove_task() -> Result<(), String> {
    let (user, _) = token_account()?;
    let name = elevated_task_name(&user);
    // Only exit codes are used because schtasks output is localized; a failed query = no task.
    match run_schtasks(&["/Query", "/TN", &name]) {
        Ok(()) => run_schtasks(&["/Delete", "/TN", &name, "/F"]),
        Err(_) => Ok(()),
    }
}

pub fn start_task() -> Result<(), String> {
    let (user, _) = token_account()?;
    run_schtasks(&["/Run", "/TN", &elevated_task_name(&user)])
}

/// Passes `target` to the already-running, unelevated Explorer.
pub(crate) fn open_unelevated(target: &Path) {
    let spawned = in_system_folder(windows_dir, "explorer.exe").and_then(|explorer| {
        Command::new(explorer)
            .arg(target)
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .map(drop)
            .map_err(|e| e.to_string())
    });
    if let Err(e) = spawned {
        log::line(&format!("admin: explorer: {e}"));
    }
}

/// True when this launch should start the elevated task and exit instead of running the core.
pub fn should_hand_off(mode_on: bool, elevated: bool, via_task: bool, in_pf: bool) -> bool {
    mode_on && in_pf && !elevated && !via_task
}

/// The saved config, read without side effects; only the core may call `config::load`.
pub fn config_on_disk() -> Config {
    match config::config_file() {
        Some(file) => config::peek(&file),
        None => Config::default(),
    }
}

fn find_core() -> Option<HWND> {
    // SAFETY: static class name; a null title matches any window of that class.
    unsafe { FindWindowW(app::CORE_CLASS, PCWSTR::null()) }.ok()
}

/// Sends WM_CLOSE to the running core and waits for its process to end. Ok when none runs.
fn stop_core() -> Result<(), String> {
    let Some(hwnd) = find_core() else {
        return Ok(());
    };
    let mut pid = 0u32;
    // SAFETY: `hwnd` was just returned by FindWindowW; `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: opens by pid with wait rights only; closed below.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .map_err(|e| format!("open core process: {e}"))?;
    // SAFETY: posting a message carries no pointers.
    let posted = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    let result = match posted {
        Err(e) => Err(format!("core did not take WM_CLOSE: {e}")),
        // SAFETY: `process` is open with SYNCHRONIZE.
        Ok(()) if unsafe { WaitForSingleObject(process, CORE_EXIT_WAIT_MS) } == WAIT_OBJECT_0 => {
            Ok(())
        }
        Ok(()) => Err("core still running after the wait".into()),
    };
    // SAFETY: closed once and not used afterwards.
    let _ = unsafe { CloseHandle(process) };
    result
}

/// Entry for `--admin-on` / `--admin-off`, which settings starts elevated. Returns the exit code.
pub fn switch_mode(enable: bool) -> i32 {
    // Disabling only deletes the task, so it skips the Program Files check.
    let exe = std::env::current_exe()
        .ok()
        .filter(|e| running_elevated() && (!enable || exe_under_program_files(e)));
    let Some(exe) = exe else {
        log::line("admin: mode switch refused, needs elevation and a Program Files install");
        return 1;
    };
    // The mode starts from the Run value at sign-in, so bring that in line with the config now.
    if let Err(e) = autostart::set_enabled(config_on_disk().autostart) {
        log::line(&format!("admin: run value: {e}"));
    }
    let outcome = if enable {
        enable_mode(&exe)
    } else {
        disable_mode(&exe)
    };
    if let Err(e) = &outcome {
        let which = if enable { "enable" } else { "disable" };
        log::line(&format!("admin: {which} failed: {e}"));
    }
    i32::from(outcome.is_err())
}

fn enable_mode(exe: &Path) -> Result<(), String> {
    install_task()?;
    let handed = stop_core().and_then(|()| start_task());
    if handed.is_err() {
        if let Err(e) = remove_task() {
            log::line(&format!("admin: undoing the task failed: {e}"));
        }
        // Whatever went wrong, the user must still end up with a running core.
        if find_core().is_none() {
            open_unelevated(exe);
        }
    }
    handed
}

fn disable_mode(exe: &Path) -> Result<(), String> {
    remove_task()?;
    // With the task gone the next start is unelevated anyway, so a failed restart is only logged.
    match stop_core() {
        Ok(()) => open_unelevated(exe),
        Err(e) => log::line(&format!("admin: restart after disable: {e}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\Swara\swara.exe";

    #[test]
    fn task_is_named_after_the_account() {
        assert_eq!(elevated_task_name("rina"), "Swara-rina");
    }

    #[test]
    fn xml_runs_highest_with_task_flag() {
        let xml = scheduler_xml(Path::new(EXE), r"DESK\rina");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<Arguments>--from-task</Arguments>"));
        assert!(xml.contains(&format!("<Command>{EXE}</Command>")));
        assert!(xml.contains(r"<UserId>DESK\rina</UserId>"));
    }

    #[test]
    fn xml_escapes_special_characters() {
        let xml = scheduler_xml(
            Path::new(r"C:\Program Files\Tom & Jerry\swara.exe"),
            r#"DESK\x<y>"z'"#,
        );
        assert!(xml.contains(r"<Command>C:\Program Files\Tom &amp; Jerry\swara.exe</Command>"));
        assert!(xml.contains(r"<UserId>DESK\x&lt;y&gt;&quot;z&apos;</UserId>"));
        assert!(xml.contains("<Triggers />"));
        assert!(!xml.contains("LogonTrigger"));
    }

    #[test]
    fn hand_off_only_in_one_case() {
        for bits in 0u8..16 {
            let [in_pf, via_task, elevated, mode_on] = [0, 1, 2, 3].map(|i| (bits >> i) & 1 == 1);
            let expected = [mode_on, elevated, via_task, in_pf] == [true, false, false, true];
            assert_eq!(
                should_hand_off(mode_on, elevated, via_task, in_pf),
                expected,
                "mode={mode_on} elevated={elevated} task={via_task} pf={in_pf}"
            );
        }
    }

    #[test]
    fn program_files_match_is_by_whole_parts() {
        let roots = [r"C:\Program Files", r"C:\Program Files (x86)"].map(PathBuf::from);
        let inside = |p: &str| under_program_files(Path::new(p), &roots);
        assert!(inside(EXE));
        assert!(inside(r"c:\PROGRAM FILES\swara\SWARA.EXE"));
        assert!(inside(r"C:\Program Files (x86)\Swara\swara.exe"));
        assert!(!inside(r"C:\Program Files Evil\swara.exe"));
        assert!(!inside(r"E:\Program Files\Swara\swara.exe"));
        assert!(!inside(r"C:\Tools\Swara\swara.exe"));
        assert!(!under_program_files(Path::new(EXE), &[]));
    }

    #[test]
    fn qualified_user_adds_domain_when_present() {
        assert_eq!(qualified_user("DESK", "rina"), r"DESK\rina");
        assert_eq!(qualified_user("", "rina"), "rina");
    }

    #[test]
    fn system_lookups_agree_with_this_session() {
        let pf = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .expect("ProgramFiles is set");
        assert!(under_program_files(
            &pf.join("probe.exe"),
            &program_files_roots()
        ));
        let (user, _) = token_account().unwrap();
        let env_user = std::env::var("USERNAME").unwrap();
        assert!(user.eq_ignore_ascii_case(&env_user));
        assert!(
            in_system_folder(system_dir, "schtasks.exe")
                .unwrap()
                .is_file()
        );
        assert!(
            in_system_folder(windows_dir, "explorer.exe")
                .unwrap()
                .is_file()
        );
    }
}
