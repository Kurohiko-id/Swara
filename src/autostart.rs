//! Sign-in start through the `Swara` value under the HKCU Run key.

use std::path::Path;
use windows::Win32::Foundation::{E_FAIL, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{Error, PCWSTR, Result, w};

const RUN_PATH: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const ENTRY: PCWSTR = w!("Swara");

/// The open Run key; the handle is closed on drop.
struct RunKey(HKEY);

impl RunKey {
    fn open(access: REG_SAM_FLAGS) -> Result<Self> {
        let mut handle = HKEY::default();
        // SAFETY: static path; on success `handle` owns a handle that `Drop` closes.
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_PATH, None, access, &mut handle) }.ok()?;
        Ok(RunKey(handle))
    }

    fn get(&self) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        let out: *mut u8 = buf.as_mut_ptr().cast();
        // SAFETY: `out` has room for `bytes` bytes and the call writes no more than that.
        let status =
            unsafe { RegQueryValueExW(self.0, ENTRY, None, None, Some(out), Some(&mut bytes)) };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = &buf[..buf.len().min(bytes as usize / 2)];
        let text = match units.split_last() {
            Some((0, rest)) => rest,
            _ => units,
        };
        Some(String::from_utf16_lossy(text))
    }

    fn put(&self, text: &str) -> Result<()> {
        let mut data = Vec::with_capacity(text.len() * 2 + 2);
        for unit in text.encode_utf16().chain([0]) {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        // SAFETY: opened with KEY_SET_VALUE; `data` is NUL-terminated UTF-16.
        unsafe { RegSetValueExW(self.0, ENTRY, None, REG_SZ, Some(&data)) }.ok()
    }

    fn remove(&self) -> Result<()> {
        // SAFETY: opened with KEY_SET_VALUE.
        let status = unsafe { RegDeleteValueW(self.0, ENTRY) };
        // A value that is already gone counts as removed.
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            status.ok()
        }
    }
}

impl Drop for RunKey {
    fn drop(&mut self) {
        // SAFETY: the handle came from `open` and is closed only here.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Quoted so the sign-in launch does not split a path that contains spaces.
pub fn run_command(exe: &Path) -> String {
    format!(r#""{}""#, exe.display())
}

pub fn set_enabled(on: bool) -> Result<()> {
    let key = RunKey::open(KEY_QUERY_VALUE | KEY_SET_VALUE)?;
    if !on {
        return key.remove();
    }
    let exe = std::env::current_exe().map_err(|e| Error::new(E_FAIL, e.to_string()))?;
    let want = run_command(&exe);
    // Every settings save ends up here; leave the registry alone when nothing changed.
    if key.get().as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    key.put(&want)
}

/// Whether the Run value exists; the installer's autostart option is what creates it.
pub fn is_enabled() -> bool {
    RunKey::open(KEY_QUERY_VALUE).is_ok_and(|key| key.get().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_wraps_path_in_quotes() {
        assert_eq!(
            run_command(Path::new(r"D:\My Apps\Swara\swara.exe")),
            r#""D:\My Apps\Swara\swara.exe""#
        );
    }
}
