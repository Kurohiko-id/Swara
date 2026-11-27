use swara::tray::Tray;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, KillTimer, MSG,
    SetTimer, WINDOW_EX_STYLE, WINDOW_STYLE, WM_TIMER,
};
use windows::core::w;

fn main() {
    // SAFETY: message-only window of a system class; destroyed before exit.
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .expect("owner window");

    let mut tray = Tray::create(owner);
    println!("tray added, in tray: {}", in_tray(owner));
    tray.notice("Swara", "tray example");
    println!("balloon shown");

    // SAFETY: thread timer, killed below.
    let done = unsafe { SetTimer(None, 0, 3_000, None) };
    let mut queued = MSG::default();
    // SAFETY: standard message loop on this thread.
    while unsafe { GetMessageW(&mut queued, None, 0, 0) }.as_bool() {
        if queued.message == WM_TIMER && queued.hwnd.is_invalid() && queued.wParam.0 == done {
            break;
        }
        // SAFETY: dispatching a message this loop just received.
        unsafe { DispatchMessageW(&queued) };
    }
    tray.remove();
    println!("tray removed, in tray: {}", in_tray(owner));
    // SAFETY: timer and window created above.
    unsafe {
        let _ = KillTimer(None, done);
        let _ = DestroyWindow(owner);
    }
}

/// Asks the shell where the icon is; fails once it is gone.
fn in_tray(owner: HWND) -> bool {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: owner,
        uID: 1,
        ..Default::default()
    };
    // SAFETY: `id` lives across the call.
    unsafe { Shell_NotifyIconGetRect(&id) }.is_ok()
}
