//! Every UI string in Indonesian and English, plus picking the language.
//! Placeholders are `{name}` tokens, filled by `fmt`.

use crate::config::Lang;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedLang {
    En,
    Id,
}

/// Config wins; on `Auto` the installer's registry choice, then the Windows UI language.
pub fn resolve(cfg: Lang, installer_reg: Option<&str>, ui_langid: u16) -> ResolvedLang {
    match cfg {
        Lang::En => ResolvedLang::En,
        Lang::Id => ResolvedLang::Id,
        Lang::Auto => match installer_reg
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("id") => ResolvedLang::Id,
            Some("en") => ResolvedLang::En,
            _ if ui_langid & 0x3FF == 0x21 => ResolvedLang::Id,
            _ => ResolvedLang::En,
        },
    }
}

/// `Software\Swara\Bahasa`, HKCU first, then HKLM (all-users install).
pub fn installer_lang() -> Option<String> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::w;
    let from_hive = |hive: HKEY| -> Option<String> {
        let mut text = [0u16; 16];
        let mut bytes = std::mem::size_of_val(&text) as u32;
        // SAFETY: both names are static wide literals; `bytes` holds the size of `text`,
        // which stays alive across the call.
        let status = unsafe {
            RegGetValueW(
                hive,
                w!("Software\\Swara"),
                w!("Bahasa"),
                RRF_RT_REG_SZ,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = text.len().min(bytes as usize / 2);
        let value = String::from_utf16(&text[..units]).ok()?;
        Some(value.trim_end_matches('\0').to_owned())
    };
    from_hive(HKEY_CURRENT_USER).or_else(|| from_hive(HKEY_LOCAL_MACHINE))
}

pub fn current(cfg: Lang) -> ResolvedLang {
    // SAFETY: no arguments; only reads a Windows setting.
    let ui = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    resolve(cfg, installer_lang().as_deref(), ui)
}

/// Substitutes every `{token}` listed in `vars`; unknown tokens are left as they are.
pub fn fmt(template: &str, vars: &[(&str, &str)]) -> String {
    let mut text = template.to_owned();
    for (token, value) in vars {
        text = text.replace(&format!("{{{token}}}"), value);
    }
    text
}

/// `Key`, `Key::ALL` and `t` all expand from the single table below, keeping them in sync.
macro_rules! keys {
    ($($name:ident => $ind:expr, $eng:expr;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Key { $($name),* }

        impl Key {
            pub const ALL: [Key; [$(Key::$name),*].len()] = [$(Key::$name),*];
        }

        pub fn t(lang: ResolvedLang, key: Key) -> &'static str {
            match (key, lang) {
                $(
                    (Key::$name, ResolvedLang::Id) => $ind,
                    (Key::$name, ResolvedLang::En) => $eng,
                )*
            }
        }
    };
}

keys! {
    // Settings sidebar
    TabKeys => "Tombol", "Keys";
    TabGuard => "Penjaga", "Guard";
    GeneralTab => "Umum", "General";
    Sections => "Bagian", "Sections";

    // Keys tab
    KeysHelp => "Hotkey sama di beberapa baris = putar bergantian.",
        "Same hotkey on several rows = cycle through them.";
    AddKey => "+ Tambah tombol", "+ Add key";
    SetKey => "Pasang tombol", "Set key";
    PressKey => "tekan…", "press…";
    Cycle => "putar {i}/{n}", "cycle {i}/{n}";
    DragHint => "Geser untuk ubah urutan", "Drag to reorder";
    SwitchTo => "Pindah ke", "Switch to";
    RemoveRow => "Hapus baris", "Remove row";
    RemoveKey => "Hapus tombol", "Remove key";
    RerecordKey => "Rekam ulang tombol", "Re-record key";
    AddAnotherKey => "Tambah tombol lain", "Add another key";
    DeviceNotConnected => "{name} (tidak terhubung)", "{name} (not connected)";
    TypedKeyWarn => "\"{key}\" juga ikut terketik di game dan chat. Numpad atau F13–F24 lebih aman.",
        "\"{key}\" also types into games and chat. Numpad or F13–F24 is safer.";
    OnSwitch => "Saat ganti", "On switch";
    ShowName => "Tampilkan nama device", "Show device name";
    ShowNameSub => "Muncul sebentar di layar, tidak mengganggu game.", "Shows briefly on screen, won't get in the way of games.";
    SwitchComms => "Ganti juga device komunikasi", "Also switch the communications device";
    SwitchCommsSub => "Matikan kalau Discord sengaja tetap di headset.", "Turn this off if you want Discord to stay on your headset.";
    DeviceNames => "Nama device", "Device names";
    NameFor => "Nama untuk {name}", "Name for {name}";
    KeyInvalid => "Tombol ini tidak bisa dipakai.", "This key can't be used.";
    DuplicatePair => "Tombol dan device ini sudah ada di baris lain.", "This key and device are already on another row.";

    // Guard tab (GuardOn is also the tray item)
    GuardOn => "Penjaga aktif", "Guard on";
    GuardOnSub => "Kalau ada app lain yang ganti device, Swara bertindak sesuai mode.", "If another app changes the device, Swara responds based on the mode.";
    Output => "Output", "Output";
    Input => "Input", "Input";
    MainDevice => "Device utama", "Main device";
    CommsDevice => "Device komunikasi", "Communications device";
    SameAsAbove => "Sama seperti di atas", "Same as above";
    GuardNone => "Tidak ada", "None";
    Mode => "Mode", "Mode";
    ModeOff => "Mati", "Off";
    ModeNotify => "Notif", "Notify";
    ModeAuto => "Auto", "Auto";
    ModeOutput => "Mode output", "Output mode";
    ModeInput => "Mode input", "Input mode";
    SnoozeLabel => "Ingetin lagi setelah", "Remind again after";
    SnoozeSub => "Dipakai saat kamu pilih \"Ingetin\" di notif.", "Used when you pick \"Remind\" in the popup.";
    Minutes => "menit", "min";
    Decrease => "Kurangi", "Decrease";
    Increase => "Tambah", "Increase";

    // General tab
    Language => "Bahasa", "Language";
    LangAuto => "Auto", "Auto";
    LangIndonesian => "Indonesia", "Indonesia";
    LangEnglish => "English", "English";
    System => "Sistem", "System";
    RunAtStartup => "Jalankan saat Windows nyala", "Start with Windows";
    RunAsAdmin => "Jalankan sebagai admin", "Run as admin";
    RunAsAdminSub => "Minta izin sekali.", "Asks for permission once.";
    AdminNeedsInstall => "Perlu install untuk semua user di Program Files.", "Needs an install for all users in Program Files.";
    AdminFailed => "Gagal mengubah mode admin.", "Couldn't change admin mode.";
    LeagueNote => "Main League of Legends? Hotkey sudah jalan di dalam game. Kalau League kamu jalan sebagai administrator, nyalakan ini supaya hotkey tetap jalan.",
        "Playing League of Legends? Hotkeys already work in game. If you run League as administrator, turn this on so hotkeys keep working.";
    UpdateSupport => "Update & dukungan", "Updates & support";
    Version => "Versi {ver}", "Version {ver}";
    CheckUpdate => "Cek update", "Check for updates";
    KofiRow => "Traktir kopi", "Buy me a coffee";
    Kofi => "Ko-fi", "Ko-fi";
    SaweriaRow => "Dukung lewat Saweria", "Support via Saweria";
    Saweria => "Saweria", "Saweria";
    TopUpRow => "Top up game kamu", "Top up your games";
    TopUp => "KurohikoTopUp.com", "KurohikoTopUp.com";
    BugRow => "Ada masalah?", "Found a problem?";
    Bug => "Lapor bug", "Report a bug";

    // Update status
    UpdateChecking => "Lagi cek update…", "Checking for updates…";
    UpdateLatest => "Sudah versi terbaru", "You're on the latest version";
    UpdateAvailable => "Versi {ver} tersedia", "Version {ver} is available";
    UpdateFailed => "Gagal cek update", "Couldn't check for updates";
    UpdateDownloading => "Lagi unduh update…", "Downloading update…";
    UpdateInstallFailed => "Gagal pasang update", "Couldn't install the update";
    InstallUpdate => "Pasang update", "Install update";

    // Settings save
    SaveBlocked => "Pengaturan tidak bisa dibaca, jadi perubahan tidak disimpan.", "Settings couldn't be read, so changes aren't saved.";
    SaveFailed => "Gagal menyimpan pengaturan.", "Couldn't save settings.";

    // Tray
    TrayDevices => "Pilih device", "Choose device";
    TraySettings => "Pengaturan", "Settings";
    TrayQuit => "Keluar", "Quit";

    // OSD
    OsdSwitched => "Pindah ke {name}", "Switched to {name}";
    OsdNotConnected => "Device tidak terhubung", "Device not connected";

    // Guard popup
    PopupOutputChanged => "Output berubah", "Output changed";
    PopupInputChanged => "Input berubah", "Input changed";
    PopupNow => "Sekarang", "Now";
    PopupYourPick => "Pilihan kamu", "Your pick";
    PopupRevert => "Balikin", "Switch back";
    PopupKeep => "Biarin", "Keep";
    PopupSnooze => "Ingetin {n} mnt", "Remind in {n} min";
    GuardPaused => "Penjaga jeda {n} mnt, ada app lain yang terus ganti device.", "Guard paused for {n} min, another app keeps changing the device.";

    // Config notices (tray balloon)
    ConfigCorrupt => "Pengaturan rusak, jadi dikembalikan ke bawaan. File lama disimpan sebagai config.json.bad.",
        "Settings were damaged and have been reset. The old file was kept as config.json.bad.";
    ConfigUnreadable => "Pengaturan tidak bisa dibaca. Swara jalan dengan pengaturan bawaan dan tidak menyimpan perubahan.",
        "Settings couldn't be read. Swara is running on defaults and won't save changes.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Lang;

    /// Words that are the same in both languages.
    const SAME: &[&str] = &[
        "Ko-fi",
        "Saweria",
        "KurohikoTopUp.com",
        "Output",
        "Input",
        "Auto",
        "Mode",
        "Indonesia",
        "English",
    ];

    fn tokens(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s
            .split('{')
            .skip(1)
            .filter_map(|p| p.split('}').next())
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn every_key_has_both_languages() {
        for k in Key::ALL {
            let (id, en) = (t(ResolvedLang::Id, k), t(ResolvedLang::En, k));
            assert!(!id.is_empty() && !en.is_empty(), "{k:?} empty");
            assert!(id != en || SAME.contains(&en), "{k:?} not translated: {en}");
            assert_eq!(tokens(id), tokens(en), "{k:?} placeholders differ");
        }
    }

    #[test]
    fn resolve_order() {
        use ResolvedLang::{En, Id};
        assert_eq!(resolve(Lang::En, Some("id"), 0x0421), En);
        assert_eq!(resolve(Lang::Id, Some("en"), 0x0409), Id);
        assert_eq!(resolve(Lang::Auto, Some("id"), 0x0409), Id);
        assert_eq!(resolve(Lang::Auto, Some(" EN "), 0x0421), En);
        assert_eq!(resolve(Lang::Auto, Some("fr"), 0x0421), Id);
        assert_eq!(resolve(Lang::Auto, None, 0x0421), Id);
        assert_eq!(resolve(Lang::Auto, None, 0x0409), En);
        assert_eq!(resolve(Lang::Auto, None, 0x0411), En);
    }

    #[test]
    fn league_text_verbatim() {
        assert_eq!(
            t(ResolvedLang::Id, Key::LeagueNote),
            "Main League of Legends? Hotkey sudah jalan di dalam game. Kalau League kamu jalan sebagai administrator, nyalakan ini supaya hotkey tetap jalan."
        );
        assert_eq!(
            t(ResolvedLang::En, Key::LeagueNote),
            "Playing League of Legends? Hotkeys already work in game. If you run League as administrator, turn this on so hotkeys keep working."
        );
    }

    #[test]
    fn fmt_replaces_tokens() {
        let args = [("i", "2"), ("n", "3")];
        assert_eq!(fmt("putar {i}/{n}", &args), "putar 2/3");
        assert_eq!(fmt("{key} {key}", &[("key", "F13")]), "F13 F13");
        assert_eq!(fmt("no {x}", &[]), "no {x}");
    }
}
