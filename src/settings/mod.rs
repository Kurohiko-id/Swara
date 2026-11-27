//! The settings window, run as its own process (`swara.exe --settings`).

mod dpi;
pub mod drag;
mod record;
mod ui;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{PCWSTR, w};

/// One settings window at a time; a second launch raises the first and exits.
pub fn run() -> i32 {
    // SAFETY: static name; the handle stays open for the process lifetime on purpose.
    let mutex = unsafe { CreateMutexW(None, false, w!(r"Local\Swara-Settings")) };
    // SAFETY: reads this thread's last error right after CreateMutexW.
    if mutex.is_ok() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        raise();
        return 0;
    }
    crate::stats::init_errors_only();
    // SAFETY: once on this thread, before any COM use (device lists).
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    ui::run()
}

/// Settings is a normal window, so taking the foreground here is allowed. No window yet means
/// the first instance is still starting.
fn raise() {
    // SAFETY: static class name; the handle comes from FindWindowW and is only passed back.
    unsafe {
        if let Ok(h) = FindWindowW(ui::WINDOW_CLASS, PCWSTR::null()) {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
    }
}
