//! Checks and labels for hotkeys, plus both ways a press reaches the core:
//! `RegisterHotKey` and a background keyboard Raw Input registration.

use crate::config::{Binding, Hotkey, MOD_NOREPEAT};
use std::mem::size_of;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    self as kbd, GetAsyncKeyState, HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_SYSKEYDOWN};

pub const MOD_ALT: u32 = kbd::MOD_ALT.0;
pub const MOD_CONTROL: u32 = kbd::MOD_CONTROL.0;
pub const MOD_SHIFT: u32 = kbd::MOD_SHIFT.0;
pub const MOD_WIN: u32 = kbd::MOD_WIN.0;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HotkeyError {
    Empty,
    ModifierOnly,
    /// Out-of-range vk or unknown modifier bits; only a hand-edited config produces these.
    Invalid,
}

pub fn check(key: &Hotkey) -> Result<(), HotkeyError> {
    match key.vk {
        0 => Err(HotkeyError::Empty),
        _ if key.vk > 0xFE || key.mods & !0xF != 0 => Err(HotkeyError::Invalid),
        0x10..=0x12 | 0xA0..=0xA5 | 0x5B..=0x5C => Err(HotkeyError::ModifierOnly),
        _ => Ok(()),
    }
}

fn key_name(vk: u32) -> Option<String> {
    let s = match vk {
        0x41..=0x5A | 0x30..=0x39 => char::from_u32(vk)?.to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x6A => "Num *".into(),
        0x6B => "Num +".into(),
        0x6D => "Num -".into(),
        0x6E => "Num .".into(),
        0x6F => "Num /".into(),
        0x20 => "Space".into(),
        0x0D => "Enter".into(),
        0xB0 => "Next Track".into(),
        0xB1 => "Previous Track".into(),
        0xB2 => "Stop".into(),
        0xB3 => "Play/Pause".into(),
        0xAD => "Volume Mute".into(),
        0xAE => "Volume Down".into(),
        0xAF => "Volume Up".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x13 => "Pause".into(),
        0x91 => "Scroll Lock".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0C => "Clear".into(),
        0x14 => "Caps Lock".into(),
        0x1B => "Esc".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "Print Screen".into(),
        0x5D => "Menu".into(),
        0x5F => "Sleep".into(),
        0x90 => "Num Lock".into(),
        0xA6 => "Browser Back".into(),
        0xA7 => "Browser Forward".into(),
        0xA8 => "Browser Refresh".into(),
        0xA9 => "Browser Stop".into(),
        0xAA => "Browser Search".into(),
        0xAB => "Browser Favorites".into(),
        0xAC => "Browser Home".into(),
        0xB4 => "Mail".into(),
        0xB5 => "Media Select".into(),
        0xB6 => "App 1".into(),
        0xB7 => "App 2".into(),
        _ => return None,
    };
    Some(s)
}

const MOD_LABELS: [(u32, &str); 4] = [
    (MOD_CONTROL, "Ctrl+"),
    (MOD_ALT, "Alt+"),
    (MOD_SHIFT, "Shift+"),
    (MOD_WIN, "Win+"),
];

pub fn label(key: &Hotkey) -> String {
    let mut text: String = MOD_LABELS
        .iter()
        .filter(|(bit, _)| key.mods & bit != 0)
        .map(|(_, prefix)| *prefix)
        .collect();
    let base = key_name(key.vk).or_else(|| layout_char(key.vk));
    text.push_str(&base.unwrap_or_else(|| format!("Key 0x{:02X}", key.vk)));
    text
}

/// OEM keys print different symbols per keyboard layout, so the active layout decides.
fn layout_char(vk: u32) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_CHAR, MapVirtualKeyW};
    // SAFETY: value-only lookup. Dead keys set the top bit, which is masked off.
    let c = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) } & 0x7FFF;
    char::from_u32(c)
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .map(|c| c.to_string())
}

/// Without modifiers, a letter, digit, Space or Enter would trigger mid-chat.
pub fn typed_key_warning(key: &Hotkey) -> bool {
    key.mods == 0 && matches!(key.vk, 0x41..=0x5A | 0x30..=0x39 | 0x20 | 0x0D)
}

/// Modifier-free keys go through Raw Input only. RegisterHotKey would eat the key system-wide,
/// whereas Raw Input lets other apps still receive it.
pub fn passes_through(key: &Hotkey) -> bool {
    key.mods == 0
}

/// A key used by more than one binding must still be registered only once.
pub fn unique_keys(bindings: &[Binding]) -> Vec<Hotkey> {
    let mut seen = Vec::new();
    bindings
        .iter()
        .flat_map(|row| row.keys.iter().copied())
        .for_each(|key| {
            if !seen.contains(&key) {
                seen.push(key);
            }
        });
    seen
}

/// Modifiers must match exactly, otherwise a bare-M binding would also catch Ctrl+M.
pub fn match_raw(vk: u32, held_mods: u32, keys: &[Hotkey]) -> Option<usize> {
    keys.iter()
        .position(|key| key.mods == held_mods && key.vk == vk)
}

fn is_held(vk: i32) -> bool {
    // SAFETY: queries key state by value; nothing is borrowed.
    let state = unsafe { GetAsyncKeyState(vk) };
    state as u16 & 0x8000 != 0
}

/// Raw Input packets lack modifier state, so it is read from the keyboard at the moment.
pub fn mods_down() -> u32 {
    let held = [
        (is_held(0x11), MOD_CONTROL),
        (is_held(0x12), MOD_ALT),
        (is_held(0x10), MOD_SHIFT),
        (is_held(0x5B) || is_held(0x5C), MOD_WIN),
    ];
    held.iter()
        .filter(|(down, _)| *down)
        .fold(0, |acc, (_, bit)| acc | bit)
}

/// `MOD_NOREPEAT` is applied only at this point so a held key cannot switch repeatedly.
pub fn register(owner: HWND, slot: i32, key: &Hotkey) -> windows::core::Result<()> {
    let mods = HOT_KEY_MODIFIERS(key.mods | MOD_NOREPEAT);
    // SAFETY: `owner` is a window this process created; no pointers are passed.
    unsafe { RegisterHotKey(Some(owner), slot, mods, key.vk) }
}

pub fn unregister(owner: HWND, slot: i32) {
    // SAFETY: `owner` is ours; an error just means `slot` was never registered.
    let _ = unsafe { UnregisterHotKey(Some(owner), slot) };
}

/// Games using RIDEV_NOHOTKEYS suppress RegisterHotKey while focused; a background keyboard
/// sink keeps receiving the keys regardless.
pub fn register_raw_sink(hwnd: HWND) -> windows::core::Result<()> {
    let keyboard = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x06,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    let entry_size = size_of::<RAWINPUTDEVICE>() as u32;
    // SAFETY: `keyboard` is a live array and `entry_size` is its element size.
    unsafe { RegisterRawInputDevices(&keyboard, entry_size) }
}

/// Keyboard WM_INPUT packet as `(vk, pressed)`; other device types give `None`.
pub fn raw_key_event(lp: LPARAM) -> Option<(u32, bool)> {
    let mut packet = RAWINPUT::default();
    let mut capacity = size_of::<RAWINPUT>() as u32;
    let header_size = size_of::<RAWINPUTHEADER>() as u32;
    // SAFETY: `lp` carries the WM_INPUT handle and `packet` is an aligned RAWINPUT bounded by
    // `capacity`, so larger HID packets make the call fail rather than write past it.
    let copied = unsafe {
        GetRawInputData(
            HRAWINPUT(lp.0 as _),
            RID_INPUT,
            Some((&raw mut packet).cast()),
            &mut capacity,
            header_size,
        )
    };
    let is_keyboard = packet.header.dwType == RIM_TYPEKEYBOARD.0;
    if copied == 0 || copied == u32::MAX || !is_keyboard {
        return None;
    }
    // SAFETY: the header confirmed the union holds a keyboard record.
    let kb = unsafe { packet.data.keyboard };
    let pressed = matches!(kb.Message, WM_KEYDOWN | WM_SYSKEYDOWN);
    Some((u32::from(kb.VKey), pressed))
}

/// Holding a key makes Raw Input resend key-down, and `MOD_NOREPEAT` has no effect there,
/// so each vk counts once until it is released.
/// Per WM_INPUT the core calls `press`, `match_raw`, then `past_dedup`; the dedup timestamp
/// is shared with WM_HOTKEY.
#[derive(Default)]
pub struct RawHeld(Vec<u32>);

impl RawHeld {
    pub fn press(&mut self, vk: u32, down: bool) -> bool {
        if !down {
            self.0.retain(|&held| held != vk);
            return false;
        }
        if self.0.contains(&vk) {
            return false;
        }
        self.0.push(vk);
        true
    }
}

/// The same press may show up via WM_HOTKEY and WM_INPUT; a repeat inside this gap is ignored.
pub const DEDUP_WINDOW: Duration = Duration::from_millis(100);

pub fn past_dedup(previous: Option<Instant>, now: Instant) -> bool {
    match previous {
        None => true,
        Some(at) => now.duration_since(at) >= DEDUP_WINDOW,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Binding, DeviceRef, Hotkey};
    use std::time::{Duration, Instant};

    fn combo(mods: u32, vk: u32) -> Hotkey {
        Hotkey { mods, vk }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn numpad_keys_get_num_prefix() {
        let cases = [
            (0x60, "Num 0"),
            (0x64, "Num 4"),
            (0x69, "Num 9"),
            (0x6A, "Num *"),
            (0x6B, "Num +"),
            (0x6D, "Num -"),
            (0x6E, "Num ."),
            (0x6F, "Num /"),
        ];
        for (vk, want) in cases {
            assert_eq!(label(&combo(0, vk)), want);
        }
    }

    #[test]
    fn modifiers_print_in_fixed_order() {
        let all = MOD_SHIFT | MOD_CONTROL | MOD_WIN | MOD_ALT;
        assert_eq!(label(&combo(all, 0x7F)), "Ctrl+Alt+Shift+Win+F16");
        assert_eq!(label(&combo(MOD_ALT | MOD_CONTROL, 0x4B)), "Ctrl+Alt+K");
        assert_eq!(label(&combo(MOD_WIN, 0x33)), "Win+3");
        assert_eq!(label(&combo(0, 0x87)), "F24");
        assert_eq!(label(&combo(MOD_ALT, 0xFF)), "Alt+Key 0xFF");
        assert_eq!(label(&combo(0, 0x2BAD)), "Key 0x2BAD");
    }

    #[test]
    fn lone_modifier_is_refused() {
        assert_eq!(check(&combo(0, 0)), Err(HotkeyError::Empty));
        assert_eq!(check(&combo(MOD_SHIFT, 0)), Err(HotkeyError::Empty));
        let modifier_vks = [
            0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C,
        ];
        for vk in modifier_vks {
            assert_eq!(check(&combo(0, vk)), Err(HotkeyError::ModifierOnly));
            assert_eq!(check(&combo(MOD_ALT, vk)), Err(HotkeyError::ModifierOnly));
        }
    }

    #[test]
    fn single_keys_without_modifiers_are_fine() {
        for vk in [0x61, 0x6B, 0x7A] {
            assert_eq!(check(&combo(0, vk)), Ok(()), "{vk:#x}");
        }
        assert_eq!(check(&combo(MOD_SHIFT | MOD_WIN, 0x4B)), Ok(()));
    }

    #[test]
    fn typing_keys_warn_only_when_bare() {
        for vk in [0x0D, 0x20, 0x30, 0x39, 0x41, 0x5A] {
            assert!(typed_key_warning(&combo(0, vk)), "{vk:#x}");
            assert!(!typed_key_warning(&combo(MOD_ALT, vk)), "{vk:#x}");
        }
        for vk in [0x60, 0x69, 0x6F, 0x70, 0x7B, 0x87] {
            assert!(!typed_key_warning(&combo(0, vk)), "{vk:#x}");
        }
    }

    #[test]
    fn shared_keys_collapse_keeping_first_seen_order() {
        let row = |mods, vk, id: &str| Binding {
            keys: vec![combo(mods, vk)],
            device: DeviceRef {
                id: id.into(),
                name: String::new(),
            },
        };
        let rows = [
            row(0, 0x6B, "spk"),
            row(MOD_ALT, 0x4B, "hp"),
            row(0, 0x6B, "tv"),
            row(0, 0x4B, "mic"),
            row(MOD_ALT, 0x4B, "usb"),
        ];
        assert_eq!(
            unique_keys(&rows),
            vec![combo(0, 0x6B), combo(MOD_ALT, 0x4B), combo(0, 0x4B)]
        );
        assert!(unique_keys(&[]).is_empty());
        let several = Binding {
            keys: vec![combo(0, 0xBA), combo(0, 0xC0), combo(0, 0xBA)],
            device: DeviceRef::default(),
        };
        assert_eq!(
            unique_keys(&[several]),
            vec![combo(0, 0xBA), combo(0, 0xC0)]
        );
    }

    #[test]
    fn raw_match_needs_identical_modifiers() {
        let keys = [
            combo(0, 0x6B),
            combo(MOD_ALT, 0x4B),
            combo(MOD_ALT | MOD_SHIFT, 0x4C),
        ];
        assert_eq!(match_raw(0x6B, 0, &keys), Some(0));
        assert_eq!(match_raw(0x6B, MOD_ALT, &keys), None);
        assert_eq!(match_raw(0x4B, MOD_ALT, &keys), Some(1));
        assert_eq!(match_raw(0x4B, 0, &keys), None);
        assert_eq!(match_raw(0x4B, MOD_ALT | MOD_CONTROL, &keys), None);
        assert_eq!(match_raw(0x4C, MOD_ALT | MOD_SHIFT, &keys), Some(2));
        assert_eq!(match_raw(0x4C, MOD_SHIFT, &keys), None);
        assert_eq!(match_raw(0x5A, 0, &keys), None);
    }

    #[test]
    fn second_press_inside_window_is_dropped() {
        let start = Instant::now();
        assert!(past_dedup(None, start));
        assert!(!past_dedup(Some(start), start));
        assert!(!past_dedup(Some(start), start + ms(60)));
        assert!(!past_dedup(Some(start), start + ms(99)));
        assert!(past_dedup(Some(start), start + DEDUP_WINDOW));
        assert!(past_dedup(Some(start), start + ms(2_000)));
    }

    #[test]
    fn held_key_counts_once_until_released() {
        let mut held = RawHeld::default();
        assert!(held.press(0x6B, true));
        assert!(!held.press(0x6B, true));
        assert!(!held.press(0x6B, true));
        assert!(!held.press(0x6B, false));
        assert!(held.press(0x6B, true));
        assert!(held.press(0x4B, true));
        assert!(!held.press(0x4B, true));
        assert!(!held.press(0x6B, true));
        assert!(!held.press(0x4B, false));
        assert!(!held.press(0x6B, true));
        assert!(held.press(0x4B, true));
    }

    #[test]
    fn vk_above_fe_is_invalid() {
        assert_eq!(check(&combo(0, 0xFE)), Ok(()));
        assert_eq!(check(&combo(0, 0x100)), Err(HotkeyError::Invalid));
        assert_eq!(check(&combo(MOD_SHIFT, 0x2BAD)), Err(HotkeyError::Invalid));
    }

    #[test]
    fn stray_modifier_bits_are_invalid() {
        assert_eq!(check(&combo(0x20, 0x4B)), Err(HotkeyError::Invalid));
        assert_eq!(
            check(&combo(MOD_ALT | MOD_NOREPEAT, 0x4B)),
            Err(HotkeyError::Invalid)
        );
        assert_eq!(
            check(&combo(MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN, 0x4B)),
            Ok(())
        );
    }

    #[test]
    fn only_modifier_free_keys_bypass_registerhotkey() {
        assert!(passes_through(&combo(0, 0xBA)));
        assert!(passes_through(&combo(0, 0x6B)));
        assert!(!passes_through(&combo(MOD_SHIFT, 0x4B)));
        assert!(!passes_through(&combo(MOD_WIN | MOD_CONTROL, 0x4B)));
    }

    #[test]
    fn real_keyboard_keys_never_fall_back_to_hex() {
        // Every layout maps the OEM key next to right Shift to some printable symbol.
        let oem = label(&combo(0, 0xBF));
        assert!(!oem.starts_with("Key 0x"), "{oem}");
        for vk in [
            0x08, 0x09, 0x14, 0x1B, 0x26, 0x27, 0x2C, 0x5D, 0x90, 0xAB, 0xB6,
        ] {
            let shown = label(&combo(0, vk));
            assert!(!shown.starts_with("Key 0x"), "{vk:#04X} -> {shown}");
        }
    }
}
