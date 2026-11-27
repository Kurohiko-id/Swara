//! Rolling text log in `%APPDATA%\Swara\logs`. A logging failure must never reach the caller.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

const LOG_NAME: &str = "swara.log";
const ROLL_AT: u64 = 1_000_000;
const ARCHIVES: usize = 3;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Picks the log folder once. Without `%APPDATA%` every later call is a no-op.
pub fn init() {
    let Some(base) = crate::config::app_dir() else {
        return;
    };
    let logs = base.join("logs");
    let _ = fs::create_dir_all(&logs);
    let _ = LOG_DIR.set(logs);
}

/// Dropped silently until `init()` has chosen a folder.
pub fn line(msg: &str) {
    if let Some(logs) = LOG_DIR.get() {
        write_entry(logs, msg);
    }
}

/// Same as `line`, and also counted in the release-build usage report.
pub fn error(msg: &str) {
    line(msg);
    crate::stats::error(msg);
}

fn stamp(t: &SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn archive(logs: &Path, i: usize) -> PathBuf {
    logs.join(format!("{LOG_NAME}.{i}"))
}

/// Moves the live file to `.1` and older copies one step up once it hits `limit` bytes.
fn roll(logs: &Path, limit: u64, archives: usize) {
    let live = logs.join(LOG_NAME);
    let size = fs::metadata(&live).map_or(0, |m| m.len());
    if size < limit {
        return;
    }
    // Errors are dropped on purpose; a stuck rename only means a longer file.
    let _ = fs::remove_file(archive(logs, archives));
    for i in (2..=archives).rev() {
        let _ = fs::rename(archive(logs, i - 1), archive(logs, i));
    }
    let _ = fs::rename(&live, archive(logs, 1));
}

fn write_entry(logs: &Path, msg: &str) {
    // Rolling renames files in a folder the user owns; an elevated process could be tricked
    // into following a planted link, so it only appends.
    if !crate::admin::running_elevated() {
        roll(logs, ROLL_AT, ARCHIVES);
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(logs.join(LOG_NAME));
    if let Ok(mut f) = file {
        // SAFETY: GetLocalTime takes nothing and returns a plain struct.
        let now = unsafe { GetLocalTime() };
        let _ = writeln!(f, "{} {msg}", stamp(&now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swara-logtest-{}-{tag}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stamp_is_iso_like_with_millis() {
        let t = SYSTEMTIME {
            wYear: 2025,
            wMonth: 3,
            wDay: 7,
            wHour: 8,
            wMinute: 4,
            wSecond: 9,
            wMilliseconds: 45,
            ..Default::default()
        };
        assert_eq!(stamp(&t), "2025-03-07T08:04:09.045");
    }

    #[test]
    fn entry_lands_at_end_of_file() {
        let dir = scratch("entry");
        write_entry(&dir, "switched");
        let text = fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        assert!(text.trim_end().ends_with(" switched"));
    }

    #[test]
    fn roll_moves_every_copy_up() {
        let dir = scratch("roll");
        fs::write(dir.join(LOG_NAME), "live-content").unwrap();
        for i in 1..=3 {
            fs::write(archive(&dir, i), format!("copy{i}")).unwrap();
        }
        roll(&dir, 8, 3);
        assert!(!dir.join(LOG_NAME).exists());
        assert_eq!(
            fs::read_to_string(archive(&dir, 1)).unwrap(),
            "live-content"
        );
        assert_eq!(fs::read_to_string(archive(&dir, 2)).unwrap(), "copy1");
        assert_eq!(fs::read_to_string(archive(&dir, 3)).unwrap(), "copy2");
        assert!(!archive(&dir, 4).exists());
    }

    #[test]
    fn roll_leaves_small_file_alone() {
        let dir = scratch("under");
        fs::write(dir.join(LOG_NAME), "tiny").unwrap();
        roll(&dir, 64, 3);
        assert!(dir.join(LOG_NAME).exists());
        assert!(!archive(&dir, 1).exists());
    }
}
