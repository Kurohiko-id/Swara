//! Workaround for winit 0.30.13 on `WM_DPICHANGED`: it applies its own rect, and when a drag
//! across monitors leaves that rect mostly on the old screen, Windows raises a nested DPI change
//! and the window gets scaled twice (rust-windowing/winit#4041).
//! Remove after Slint upgrades to winit 0.31 or later.

use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::winit::window::Window;
use std::cell::Cell;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS, WM_DPICHANGED, WM_WINDOWPOSCHANGING,
};

const SUBCLASS_ID: usize = 1;

thread_local! {
    static TARGET_RECT: Cell<Option<RECT>> = const { Cell::new(None) };
}

/// Forces any position change winit makes inside `WM_DPICHANGED` onto Windows' suggested rect.
pub fn install(win: &Window) -> bool {
    let raw = win.window_handle().map(|handle| handle.as_raw());
    let Ok(RawWindowHandle::Win32(win32)) = raw else {
        return false;
    };
    let hwnd = HWND(win32.hwnd.get() as _);
    // SAFETY: `hwnd` belongs to this live window and `dpi_hook` matches SUBCLASSPROC.
    unsafe { SetWindowSubclass(hwnd, Some(dpi_hook), SUBCLASS_ID, 0) }.as_bool()
}

unsafe extern "system" fn dpi_hook(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass: usize,
    _ref_data: usize,
) -> LRESULT {
    let has_payload = lparam.0 != 0;
    if msg == WM_DPICHANGED && has_payload {
        // SAFETY: during this message lParam is the RECT Windows recommends.
        let suggested = unsafe { *(lparam.0 as *const RECT) };
        TARGET_RECT.with(|slot| slot.set(Some(suggested)));
        // SAFETY: passes the message on untouched.
        let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        TARGET_RECT.with(|slot| slot.set(None));
        return result;
    }
    if msg == WM_WINDOWPOSCHANGING
        && has_payload
        && let Some(target) = TARGET_RECT.with(Cell::get)
    {
        // SAFETY: during this message lParam is a WINDOWPOS we may edit.
        let change = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
        change.x = target.left;
        change.y = target.top;
        change.cx = target.right - target.left;
        change.cy = target.bottom - target.top;
        change.flags.0 &= !(SWP_NOMOVE.0 | SWP_NOSIZE.0);
    }
    // SAFETY: passes the message on untouched.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Top-left for a window of logical size `w` x `h` centred on the work area of the monitor the
/// cursor is on, scaled by that monitor's DPI so it does not jump after opening.
pub fn centered(w: f32, h: f32) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    use windows::Win32::UI::HiDpi::{
        AdjustWindowRectExForDpi, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, WINDOW_EX_STYLE, WS_CAPTION, WS_MINIMIZEBOX, WS_SYSMENU,
    };
    const INFO_BYTES: u32 = size_of::<MONITORINFO>() as u32;
    // Matches the real window: title bar with close and minimise, not resizable.
    let frame_style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut cursor = POINT::default();
    let mut info = MONITORINFO {
        cbSize: INFO_BYTES,
        ..Default::default()
    };
    let mut dpi = (96u32, 96u32);
    // SAFETY: read-only Win32 queries; every out pointer is a live local.
    let outer = unsafe {
        GetCursorPos(&mut cursor).ok()?;
        let screen = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(screen, &mut info).as_bool().then_some(())?;
        GetDpiForMonitor(screen, MDT_EFFECTIVE_DPI, &mut dpi.0, &mut dpi.1).ok()?;
        let scale = dpi.0 as f32 / 96.0;
        let mut rect = RECT {
            right: (w * scale).round() as i32,
            bottom: (h * scale).round() as i32,
            ..Default::default()
        };
        AdjustWindowRectExForDpi(&mut rect, frame_style, false, WINDOW_EX_STYLE(0), dpi.0).ok()?;
        rect
    };
    let area = info.rcWork;
    let middle = |start: i32, end: i32, span: i32| (start + (end - start - span) / 2).max(start);
    Some((
        middle(area.left, area.right, outer.right - outer.left),
        middle(area.top, area.bottom, outer.bottom - outer.top),
    ))
}
