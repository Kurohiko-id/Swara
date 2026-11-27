//! Hotkey recording over keyboard Raw Input, delivered to a hidden window on the UI thread.
//! The sink is registered only while a key box is recording.

use crate::audio::WM_APP_DEVICES;
use crate::config::Hotkey;
use crate::hotkey;
use std::cell::Cell;
use std::mem::size_of_val;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetForegroundWindow, GetWindowThreadProcessId, RegisterClassW,
    WINDOW_EX_STYLE, WM_INPUT, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SinkEvent {
    Key(Hotkey),
    Cancel,
    /// The audio endpoint list or a default changed.
    Devices,
}

/// Esc cancels; modifiers alone keep waiting (they are read from `mods` with the next key).
pub fn event(vk: u32, mods: u32) -> Option<SinkEvent> {
    match vk {
        0x1B => Some(SinkEvent::Cancel),
        0 | 0xFF | 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C => None,
        _ => Some(SinkEvent::Key(Hotkey { mods, vk })),
    }
}

thread_local! {
    static RECORDING: Cell<bool> = const { Cell::new(false) };
    static HANDLER: Cell<Option<fn(SinkEvent)>> = const { Cell::new(None) };
}

/// Runs `f` on the UI thread for every sink event.
pub fn set_handler(f: fn(SinkEvent)) {
    HANDLER.with(|h| h.set(Some(f)));
}

/// Deferred to the event loop so the handler never runs inside this window procedure.
fn post(ev: SinkEvent) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(f) = HANDLER.with(Cell::get) {
            f(ev);
        }
    });
}

/// Raw Input also reports keys typed into other programs, so only accept a key while the
/// settings window itself is in front.
fn settings_in_front() -> bool {
    let mut owner_pid = 0u32;
    // SAFETY: read-only queries; `owner_pid` is a live local.
    let thread = unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut owner_pid)) };
    // SAFETY: no arguments, cannot fail.
    thread != 0 && owner_pid == unsafe { GetCurrentProcessId() }
}

unsafe extern "system" fn sink_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_APP_DEVICES {
        post(SinkEvent::Devices);
    } else if message == WM_INPUT
        && RECORDING.with(Cell::get)
        && let Some((vk, true)) = hotkey::raw_key_event(lparam)
        && settings_in_front()
        && let Some(found) = event(vk, hotkey::mods_down())
    {
        RECORDING.with(|on| on.set(false));
        post(found);
    }
    // SAFETY: our own window; WM_INPUT must still reach the default handler to free its data.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Invisible window owned by the calling thread; Slint's loop pumps its messages.
pub fn create_sink() -> windows::core::Result<HWND> {
    const SINK_CLASS: PCWSTR = w!("SwaraSettingsSink");
    // SAFETY: a null name returns this exe's module.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(sink_proc),
        hInstance: module.into(),
        lpszClassName: SINK_CLASS,
        ..Default::default()
    };
    // SAFETY: `class` is complete and `sink_proc` is a static function.
    unsafe { RegisterClassW(&class) };
    // SAFETY: class registered above; no parent, no extra data.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            SINK_CLASS,
            PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
    }
}

pub fn start(sink: HWND) -> windows::core::Result<()> {
    hotkey::register_raw_sink(sink)?;
    RECORDING.with(|r| r.set(true));
    Ok(())
}

pub fn stop() {
    RECORDING.with(|r| r.set(false));
    // This removes winit's keyboard registration as well, which is fine: Slint ignores device
    // events.
    let keyboard = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x06,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    let entry = size_of_val(&keyboard) as u32;
    // SAFETY: one-element slice with its true size; an error only means nothing was registered.
    let _ = unsafe { RegisterRawInputDevices(&[keyboard], entry) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_cancels_and_modifiers_wait() {
        assert_eq!(event(0x1B, 4), Some(SinkEvent::Cancel));
        for modifier in [0xFF, 0x5C, 0x5B, 0xA4, 0xA1, 0x12, 0x11, 0x10, 0] {
            assert_eq!(event(modifier, 1), None);
        }
        let hk = Hotkey { mods: 5, vk: 0x77 };
        assert_eq!(event(0x77, 5), Some(SinkEvent::Key(hk)));
    }
}
