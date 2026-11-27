//! Tray icon with its right-click menu and info balloons.

use crate::i18n::{Key, ResolvedLang, t};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{LIM_SMALL, LoadIconMetric};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_RESPECT_QUIET_TIME,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, HICON, HMENU, IDI_APPLICATION,
    LoadIconW, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    WM_APP, WM_NULL,
};
use windows::core::{PCWSTR, w};

pub const WM_APP_TRAY: u32 = WM_APP + 3;
const TRAY_UID: u32 = 1;
/// winresource stores the `set_icon` icon under this id.
const ICON_RESOURCE: usize = 1;
const DEFAULT_TIP: &str = "Swara";
const NID_BYTES: u32 = size_of::<NOTIFYICONDATAW>() as u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuCmd {
    Switch(usize),
    ToggleGuard,
    Settings,
    Quit,
}

/// Item ids run 1..=outputs for devices, then guard, settings and quit. 0 is "closed without
/// picking".
fn command_for(item: u32, outputs: usize) -> Option<MenuCmd> {
    let index = usize::try_from(item).ok()?.checked_sub(1)?;
    if let Some(after) = index.checked_sub(outputs) {
        return [MenuCmd::ToggleGuard, MenuCmd::Settings, MenuCmd::Quit]
            .get(after)
            .copied();
    }
    Some(MenuCmd::Switch(index))
}

/// Explorer broadcasts `TaskbarCreated` after it restarts; the caller answers with `show`.
pub fn taskbar_created_msg() -> u32 {
    // SAFETY: constant, NUL-terminated message name.
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

pub struct Tray {
    owner: HWND,
    /// Lives until exit and is never freed; the fallback is a shared stock icon anyway.
    icon: HICON,
    hover_text: String,
}

impl Tray {
    pub fn create(owner: HWND) -> Self {
        let mut tray = Self {
            owner,
            icon: load_icon(),
            hover_text: DEFAULT_TIP.to_owned(),
        };
        tray.show();
        tray
    }

    fn identity(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: NID_BYTES,
            hWnd: self.owner,
            uID: TRAY_UID,
            ..Default::default()
        }
    }

    fn icon_data(&self) -> NOTIFYICONDATAW {
        let mut data = self.identity();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon;
        fill_wide(&mut data.szTip, &self.hover_text);
        data
    }

    fn send(message: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
        // SAFETY: `data` is a complete struct borrowed for the duration of the call.
        unsafe { Shell_NotifyIconW(message, data) }.as_bool()
    }

    /// While a guard prompt waits the icon stays the same, but the tooltip starts with "! ".
    pub fn set_alert(&mut self, on: bool, tooltip: &str) {
        let marker = if on { "! " } else { "" };
        self.hover_text = format!("{marker}{tooltip}");
        if !Self::send(NIM_MODIFY, &self.icon_data()) {
            crate::log::line("tray tooltip not updated");
        }
    }

    pub fn show(&mut self) {
        let mut data = self.icon_data();
        if !Self::send(NIM_ADD, &data) {
            crate::log::error("tray icon not added");
        }
        // With version 4 the mouse message arrives in the low word of lParam.
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Self::send(NIM_SETVERSION, &data);
    }

    pub fn notice(&self, title: &str, body: &str) {
        let mut data = self.identity();
        data.uFlags = NIF_SHOWTIP | NIF_INFO;
        data.dwInfoFlags = NIIF_RESPECT_QUIET_TIME | NIIF_INFO;
        fill_wide(&mut data.szInfoTitle, title);
        fill_wide(&mut data.szInfo, body);
        if !Self::send(NIM_MODIFY, &data) {
            crate::log::line("tray notice not shown");
        }
    }

    /// Pops the menu up at the mouse pointer and returns once it is dismissed.
    pub fn menu(
        &self,
        anchor: HWND,
        lang: ResolvedLang,
        outputs: &[(String, bool)],
        guard_on: bool,
    ) -> Option<MenuCmd> {
        let count = outputs.len();
        // SAFETY: the menu handle never leaves this block and is destroyed before returning.
        let picked = unsafe {
            let popup = CreatePopupMenu().ok()?;
            add_item(popup, MF_GRAYED, 0, t(lang, Key::TrayDevices));
            for (slot, (name, active)) in outputs.iter().enumerate() {
                add_item(popup, tick(*active), slot + 1, name);
            }
            let _ = AppendMenuW(popup, MF_SEPARATOR, 0, None);
            add_item(popup, tick(guard_on), count + 1, t(lang, Key::GuardOn));
            add_item(popup, MF_STRING, count + 2, t(lang, Key::TraySettings));
            add_item(popup, MF_STRING, count + 3, t(lang, Key::TrayQuit));

            let mut at = POINT::default();
            let _ = GetCursorPos(&mut at);
            // Only foreground call in the app, and only after a tray click: TrackPopupMenu
            // needs it or the menu stays open when the user clicks away.
            let _ = SetForegroundWindow(anchor);
            let flags = TPM_RIGHTBUTTON | TPM_RETURNCMD;
            let item = TrackPopupMenu(popup, flags, at.x, at.y, None, anchor, None);
            // Also from the TrackPopupMenu docs: a dummy message so the next click reopens it.
            let _ = PostMessageW(Some(anchor), WM_NULL, WPARAM::default(), LPARAM::default());
            let _ = DestroyMenu(popup);
            item.0
        };
        command_for(u32::try_from(picked).unwrap_or(0), count)
    }

    pub fn remove(&mut self) {
        // Identity alone (owner + uid) is enough to delete; a missing icon is not an error.
        Self::send(NIM_DELETE, &self.identity());
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

fn tick(on: bool) -> MENU_ITEM_FLAGS {
    match on {
        true => MF_CHECKED,
        false => MF_STRING,
    }
}

/// # Safety
/// `popup` must be a menu that has not been destroyed.
unsafe fn add_item(popup: HMENU, extra: MENU_ITEM_FLAGS, item: usize, text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    // SAFETY: `popup` is live per the contract; `wide` is terminated and outlives the call.
    let _ = unsafe { AppendMenuW(popup, extra | MF_STRING, item, PCWSTR(wide.as_ptr())) };
}

fn load_icon() -> HICON {
    // MAKEINTRESOURCE: the resource id travels as a fake string pointer.
    let resource = PCWSTR(ICON_RESOURCE as *const u16);
    // SAFETY: our own module, an integer resource id, and a stock icon as fallback.
    let loaded = unsafe {
        // LIM_SMALL picks the 16/20/24 px art made for the tray rather than a scaled 32 px one.
        GetModuleHandleW(PCWSTR::null())
            .and_then(|module| LoadIconMetric(Some(module.into()), resource, LIM_SMALL))
    };
    loaded
        .or_else(|e| {
            crate::log::error(&format!("tray icon resource missing: {e}"));
            // SAFETY: shared system icon, no module needed.
            unsafe { LoadIconW(None, IDI_APPLICATION) }
        })
        .unwrap_or_default()
}

/// Writes `text` into a fixed UTF-16 field, always NUL-terminated and cut to fit.
fn fill_wide(field: &mut [u16], text: &str) {
    let room = field.len() - 1;
    let mut len = 0;
    for (cell, unit) in field.iter_mut().zip(text.encode_utf16().take(room)) {
        *cell = unit;
        len += 1;
    }
    // A cut between surrogates would leave half a character; drop the lone high half.
    let lone_high = |u: u16| (0xD800..=0xDBFF).contains(&u);
    if len == room && len > 0 && lone_high(field[len - 1]) {
        len -= 1;
    }
    field[len] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_resolve_to_commands() {
        assert_eq!(command_for(1, 2), Some(MenuCmd::Switch(0)));
        assert_eq!(command_for(2, 2), Some(MenuCmd::Switch(1)));
        assert_eq!(command_for(3, 2), Some(MenuCmd::ToggleGuard));
        assert_eq!(command_for(4, 2), Some(MenuCmd::Settings));
        assert_eq!(command_for(5, 2), Some(MenuCmd::Quit));
        assert_eq!(command_for(1, 0), Some(MenuCmd::ToggleGuard));
    }

    #[test]
    fn dismissed_or_out_of_range_ids_give_nothing() {
        assert_eq!(command_for(0, 2), None);
        assert_eq!(command_for(6, 2), None);
        assert_eq!(command_for(u32::MAX, 4), None);
    }
}
