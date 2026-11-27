//! `%APPDATA%\Swara\config.json`, shared by the core and the settings window.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MOD_NOREPEAT: u32 = 0x4000;

/// `mods` holds Win32 `MOD_*` bits; `MOD_NOREPEAT` is added at registration, never stored.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    /// A modifier on its own (Shift, Ctrl, Alt, Win) cannot be a hotkey.
    fn usable(self) -> bool {
        !matches!(self.vk, 0 | 0x10..=0x12 | 0xA0..=0xA5 | 0x5B..=0x5C)
    }
}

/// `name` is the friendly name Windows reported, kept to label a device that is unplugged.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct DeviceRef {
    pub id: String,
    pub name: String,
}

/// A device plus the keys that select it. The same key on several rows cycles between them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(from = "BindingOnDisk")]
pub struct Binding {
    pub keys: Vec<Hotkey>,
    pub device: DeviceRef,
}

/// Early files stored one `key` per row instead of a `keys` list.
#[derive(Deserialize, Default)]
#[serde(default)]
struct BindingOnDisk {
    keys: Option<Vec<Hotkey>>,
    key: Option<Hotkey>,
    device: DeviceRef,
}

impl From<BindingOnDisk> for Binding {
    fn from(disk: BindingOnDisk) -> Self {
        let keys = match (disk.keys, disk.key) {
            (Some(list), _) => list,
            (None, one) => one.into_iter().collect(),
        };
        Binding {
            keys,
            device: disk.device,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    #[default]
    Off,
    Notify,
    AutoRevert,
}

/// `comms: None` follows whatever `default` is set to.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct GuardSide {
    #[serde(deserialize_with = "reset_if_bad")]
    pub default: Option<DeviceRef>,
    #[serde(deserialize_with = "reset_if_bad")]
    pub comms: Option<DeviceRef>,
    #[serde(deserialize_with = "reset_if_bad")]
    pub mode: GuardMode,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    #[default]
    Auto,
    En,
    Id,
}

/// An unknown value, say a variant added by a newer build, falls back for that field alone.
fn reset_if_bad<'de, De, V>(de: De) -> Result<V, De::Error>
where
    De: Deserializer<'de>,
    V: Deserialize<'de> + Default,
{
    V::deserialize(de).or_else(|_| Ok(V::default()))
}

/// A broken row is skipped so the rest of the list survives.
fn lenient_bindings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Binding>, D::Error> {
    let rows = Vec::<serde_json::Value>::deserialize(d).unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter_map(|row| Binding::deserialize(row).ok())
        .collect())
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    #[serde(deserialize_with = "reset_if_bad")]
    pub lang: Lang,
    /// Listed in cycle order.
    #[serde(deserialize_with = "lenient_bindings")]
    pub bindings: Vec<Binding>,
    pub also_comms: bool,
    pub osd: bool,
    /// Device id to the user's own label.
    pub nicknames: BTreeMap<String, String>,
    /// One switch that pauses both guards.
    pub guard_enabled: bool,
    pub guard_out: GuardSide,
    pub guard_in: GuardSide,
    pub snooze_min: u32,
    pub autostart: bool,
    pub run_as_admin: bool,
    /// Seconds since the Unix epoch.
    pub last_update_check: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lang: Lang::Auto,
            bindings: Vec::new(),
            also_comms: true,
            osd: true,
            nicknames: BTreeMap::new(),
            guard_enabled: true,
            guard_out: GuardSide::default(),
            guard_in: GuardSide::default(),
            snooze_min: 15,
            autostart: true,
            run_as_admin: false,
            last_update_check: 0,
        }
    }
}

impl Config {
    pub fn cleaned(mut self) -> Config {
        self.snooze_min = self.snooze_min.clamp(1, 120);
        // Each key+device pair is kept once over the whole list; rows with no key left go.
        let mut taken = HashSet::new();
        for row in &mut self.bindings {
            let id = row.device.id.clone();
            row.keys.retain_mut(|k| {
                k.mods &= !MOD_NOREPEAT;
                k.usable() && taken.insert((k.mods, k.vk, id.clone()))
            });
        }
        self.bindings.retain(|row| !row.keys.is_empty());
        self
    }
}

/// `%APPDATA%\Swara`, or `None` when the variable is missing so nothing is written to a
/// relative path.
pub fn app_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    (!appdata.is_empty()).then(|| PathBuf::from(appdata).join("Swara"))
}

pub fn config_file() -> Option<PathBuf> {
    Some(app_dir()?.join("config.json"))
}

#[derive(PartialEq, Debug)]
pub enum LoadOutcome {
    Fresh(Config),
    Loaded(Config),
    /// The file could not be parsed and now sits at `config.json.bad`.
    Corrupt(Config),
    /// Any IO error but not-found. The file is untouched and must not be overwritten.
    Unreadable(Config),
}

/// `file` with `suffix` added to its name, in the same folder.
fn with_suffix(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    file.with_file_name(name)
}

/// Far above any real config or stats file. Another process can write these, and the core
/// reads them again on every reload request.
pub(crate) const READ_CAP: u64 = 1 << 20;

/// Stands in for an IO result when `admin::as_user` could not run the operation.
fn no_user_rights() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "user token unavailable")
}

/// `None` when the file is bigger than `READ_CAP`; only that much is ever read.
pub(crate) fn read_capped(file: &Path) -> io::Result<Option<Vec<u8>>> {
    crate::admin::as_user(|| read_capped_direct(file)).unwrap_or_else(|| Err(no_user_rights()))
}

fn read_capped_direct(file: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut raw = Vec::new();
    fs::File::open(file)?
        .take(READ_CAP + 1)
        .read_to_end(&mut raw)?;
    Ok((raw.len() as u64 <= READ_CAP).then_some(raw))
}

/// The read and a `.bad` rename run with the user's rights (see `admin::as_user`).
pub fn load(file: &Path) -> LoadOutcome {
    load_with(file, |f| crate::admin::as_user(f))
}

/// `run` gives `None` when it may not touch the file: defaults then, nothing renamed, and
/// `Unreadable` keeps the file from being overwritten.
fn load_with(
    file: &Path,
    run: impl FnOnce(&dyn Fn() -> LoadOutcome) -> Option<LoadOutcome>,
) -> LoadOutcome {
    run(&|| load_direct(file)).unwrap_or(LoadOutcome::Unreadable(Config::default()))
}

fn load_direct(file: &Path) -> LoadOutcome {
    let raw = match read_capped_direct(file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return LoadOutcome::Fresh(Config::default());
        }
        Err(_) => return LoadOutcome::Unreadable(Config::default()),
    };
    // An oversized file counts as unparsable: it is kept as `.bad`, like broken JSON.
    if let Some(cfg) = raw.as_deref().and_then(decode) {
        return LoadOutcome::Loaded(cfg);
    }
    // On Windows this rename also replaces an earlier `.bad`.
    let _ = fs::rename(file, with_suffix(file, ".bad"));
    LoadOutcome::Corrupt(Config::default())
}

/// `load` for every process except the core: any error gives defaults and nothing is renamed.
pub fn peek(file: &Path) -> Config {
    match read_capped(file) {
        Ok(Some(raw)) => decode(&raw).unwrap_or_default(),
        _ => Config::default(),
    }
}

/// `None` only when the text is not a JSON object; a bad value falls back for its key alone.
fn decode(raw: &[u8]) -> Option<Config> {
    // Notepad and some editors prepend a UTF-8 BOM.
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(raw) else {
        return None;
    };
    // Start from the defaults and take each user key only if the result still deserializes.
    let defaults = Config::default();
    let mut accepted = serde_json::to_value(&defaults).ok()?;
    for (key, value) in fields {
        let mut candidate = accepted.clone();
        candidate[key.as_str()] = value;
        if Config::deserialize(&candidate).is_ok() {
            accepted = candidate;
        }
    }
    Config::deserialize(accepted).ok().map(Config::cleaned)
}

pub fn save(file: &Path, cfg: &Config) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(file, &json)
}

/// Writes a temp file and renames it over the target, so a reader never sees half a file.
/// Runs with the user's rights (see `admin::as_user`); skipped with an error if it cannot.
pub(crate) fn write_atomic(file: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::admin::as_user(|| write_atomic_direct(file, bytes))
        .unwrap_or_else(|| Err(no_user_rights()))
}

fn write_atomic_direct(file: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(folder) = file.parent() {
        fs::create_dir_all(folder)?;
    }
    // The pid keeps the core and settings apart when both save at once.
    let staging = with_suffix(file, &format!(".{}.tmp", std::process::id()));
    let written = fs::File::create(&staging)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&staging, file));
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

/// Guard fields the settings window edited in this session.
#[derive(Clone, Copy, Default, Debug)]
pub struct Touched {
    pub guard_out: bool,
    pub guard_in: bool,
    pub guard_enabled: bool,
}

/// While settings is open the core can change guard targets or the master switch; those
/// newer values win unless the user changed the same field here.
pub fn merge_for_save(disk: &Config, edited: &Config, touched: Touched) -> Config {
    let mut out = edited.clone();
    for (edited_here, mine, theirs) in [
        (touched.guard_out, &mut out.guard_out, &disk.guard_out),
        (touched.guard_in, &mut out.guard_in, &disk.guard_in),
    ] {
        if !edited_here {
            mine.default.clone_from(&theirs.default);
            mine.comms.clone_from(&theirs.comms);
        }
    }
    if !touched.guard_enabled {
        out.guard_enabled = disk.guard_enabled;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "config.json";
    const BAD: &str = "config.json.bad";

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swara-cfgtest-{}-{tag}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn device(id: &str) -> DeviceRef {
        DeviceRef {
            id: id.to_owned(),
            name: id.to_owned(),
        }
    }

    fn row(keys: &[u32], id: &str) -> Binding {
        Binding {
            keys: keys.iter().map(|&vk| Hotkey { mods: 0x6, vk }).collect(),
            device: device(id),
        }
    }

    fn pick(id: &str) -> Option<DeviceRef> {
        Some(device(id))
    }

    #[test]
    fn default_values() {
        let c = Config::default();
        assert!(c.also_comms && c.osd && c.guard_enabled && c.autostart && !c.run_as_admin);
        assert_eq!(c.snooze_min, 15);
        assert_eq!(c.guard_out.mode, GuardMode::Off);
        assert!(c.bindings.is_empty());
    }

    #[test]
    fn broken_values_reset_one_field_each() {
        let c = decode(br#"{"snooze_min":[],"osd":false,"unknown_key":true}"#).unwrap();
        assert_eq!(c.snooze_min, 15);
        assert!(!c.osd);
        let c =
            decode(br#"{"lang":"de","guard_out":{"mode":"shout"},"also_comms":false}"#).unwrap();
        assert_eq!(c.lang, Lang::Auto);
        assert_eq!(c.guard_out.mode, GuardMode::Off);
        assert!(!c.also_comms);
        assert_eq!(decode(b"\"text\""), None);
        assert_eq!(decode(b"[]"), None);
        assert_eq!(decode(b"{"), None);
    }

    #[test]
    fn cleaned_drops_repeat_pairs_and_modifiers() {
        let k = 0x4B;
        let c = Config {
            bindings: vec![
                row(&[k], "P"),
                row(&[k], "P"),
                row(&[k], "Q"),
                row(&[0], "P"),
                row(&[0x11], "R"),
                row(&[0xA4], "R"),
                row(&[0x5C], "R"),
            ],
            ..Default::default()
        }
        .cleaned();
        assert_eq!(c.bindings, vec![row(&[k], "P"), row(&[k], "Q")]);
    }

    #[test]
    fn snooze_kept_in_range() {
        for (given, kept) in [(0, 1), (999, 120), (45, 45)] {
            let c = Config {
                snooze_min: given,
                ..Default::default()
            }
            .cleaned();
            assert_eq!(c.snooze_min, kept);
        }
    }

    #[test]
    fn unparsable_file_moves_to_bad() {
        let dir = fresh_dir("corrupt");
        let file = dir.join(NAME);
        fs::write(dir.join(BAD), "previous").unwrap();
        fs::write(&file, "not json").unwrap();
        assert_eq!(load(&file), LoadOutcome::Corrupt(Config::default()));
        assert!(!file.exists());
        assert_eq!(fs::read_to_string(dir.join(BAD)).unwrap(), "not json");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_file_moves_to_bad_unread() {
        let dir = fresh_dir("oversized");
        let file = dir.join(NAME);
        // Valid JSON padded past the cap: refused on size, not content.
        let mut big = b"{\"snooze_min\": 45}".to_vec();
        big.resize(READ_CAP as usize + 1, b' ');
        fs::write(&file, &big).unwrap();
        assert_eq!(peek(&file), Config::default());
        assert!(file.exists(), "peek must not rename");
        assert_eq!(load(&file), LoadOutcome::Corrupt(Config::default()));
        assert!(!file.exists());
        assert_eq!(fs::metadata(dir.join(BAD)).unwrap().len(), READ_CAP + 1);
        // Right at the cap is still read.
        big.truncate(READ_CAP as usize);
        fs::write(&file, &big).unwrap();
        assert!(matches!(load(&file), LoadOutcome::Loaded(c) if c.snooze_min == 45));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_without_user_rights_leaves_file_alone() {
        let dir = fresh_dir("norights");
        let file = dir.join(NAME);
        fs::write(&file, "not json").unwrap();
        let got = load_with(&file, |_| None);
        assert_eq!(got, LoadOutcome::Unreadable(Config::default()));
        assert_eq!(fs::read_to_string(&file).unwrap(), "not json");
        assert!(!dir.join(BAD).exists());
        // The same file through a runner that may act is renamed as usual.
        assert_eq!(
            load_with(&file, |f| Some(f())),
            LoadOutcome::Corrupt(Config::default())
        );
        assert!(dir.join(BAD).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn absent_file_is_fresh() {
        let dir = fresh_dir("absent");
        let got = load(&dir.join(NAME));
        assert_eq!(got, LoadOutcome::Fresh(Config::default()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_then_load_gives_same_config() {
        let dir = fresh_dir("cycle");
        let file = dir.join(NAME);
        let saved = Config {
            lang: Lang::Id,
            bindings: vec![row(&[0x4B], "P")],
            nicknames: BTreeMap::from([("P".to_owned(), "Headset".to_owned())]),
            guard_out: GuardSide {
                default: pick("P"),
                comms: None,
                mode: GuardMode::AutoRevert,
            },
            snooze_min: 45,
            last_update_check: 1_650_000_000,
            ..Default::default()
        };
        save(&file, &saved).unwrap();
        let got = load(&file);
        assert_eq!(got, LoadOutcome::Loaded(saved));
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [NAME], "temp file left behind");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peek_leaves_bad_file_in_place() {
        let dir = fresh_dir("peek");
        let file = dir.join(NAME);
        fs::write(&file, "not json").unwrap();
        assert_eq!(peek(&file), Config::default());
        assert!(file.exists());
        assert!(!dir.join(BAD).exists());
        fs::write(&file, r#"{"run_as_admin": true, "snooze_min": 0}"#).unwrap();
        let c = peek(&file);
        assert!(c.run_as_admin);
        assert_eq!(c.snooze_min, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_strips_bom_and_cleans() {
        let dir = fresh_dir("bom");
        let file = dir.join(NAME);
        fs::write(&file, b"\xEF\xBB\xBF{\"snooze_min\": 500}").unwrap();
        let LoadOutcome::Loaded(c) = load(&file) else {
            panic!("BOM file did not load");
        };
        assert_eq!(c.snooze_min, 120);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merge_prefers_core_output_target() {
        let mut disk = Config::default();
        disk.guard_out.default = pick("Y");
        disk.guard_out.comms = pick("Y-chat");
        let mut edited = Config {
            osd: false,
            snooze_min: 33,
            ..Default::default()
        };
        edited.guard_out.default = pick("X");
        edited.guard_out.comms = pick("X-chat");
        edited.guard_out.mode = GuardMode::Notify;

        let merged = merge_for_save(&disk, &edited, Touched::default());
        assert_eq!(merged.guard_out.default, pick("Y"));
        assert_eq!(merged.guard_out.comms, pick("Y-chat"));
        let mut want = edited.clone();
        want.guard_out.default = pick("Y");
        want.guard_out.comms = pick("Y-chat");
        assert_eq!(merged, want);

        let everything = Touched {
            guard_out: true,
            guard_in: true,
            guard_enabled: true,
        };
        assert_eq!(merge_for_save(&disk, &edited, everything), edited);
    }

    #[test]
    fn merge_prefers_core_input_target_and_switch() {
        let mut disk = Config {
            guard_enabled: false,
            ..Default::default()
        };
        disk.guard_in.default = pick("Mic-1");
        disk.guard_in.comms = pick("Mic-1c");
        let mut edited = Config::default();
        edited.guard_in.default = pick("Mic-2");
        edited.guard_in.comms = pick("Mic-2c");
        edited.guard_in.mode = GuardMode::Notify;

        let merged = merge_for_save(&disk, &edited, Touched::default());
        assert_eq!(merged.guard_in.default, pick("Mic-1"));
        assert_eq!(merged.guard_in.comms, pick("Mic-1c"));
        assert_eq!(merged.guard_in.mode, GuardMode::Notify);
        assert!(!merged.guard_enabled);

        let input_only = Touched {
            guard_in: true,
            ..Default::default()
        };
        let merged = merge_for_save(&disk, &edited, input_only);
        assert_eq!(merged.guard_in, edited.guard_in);
        assert!(!merged.guard_enabled);

        let switch_only = Touched {
            guard_enabled: true,
            ..Default::default()
        };
        let merged = merge_for_save(&disk, &edited, switch_only);
        assert!(merged.guard_enabled);
        assert_eq!(merged.guard_in.default, pick("Mic-1"));
    }

    #[test]
    fn io_error_keeps_file_and_name() {
        let dir = fresh_dir("ioerr");
        let file = dir.join(NAME);
        // A folder where the file should be makes the read fail with something other than
        // not-found.
        fs::create_dir_all(&file).unwrap();
        assert_eq!(load(&file), LoadOutcome::Unreadable(Config::default()));
        assert!(file.exists());
        assert!(!dir.join(BAD).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_row_does_not_drop_neighbours() {
        let c = decode(
            br#"{"bindings":[
                {"key":{"mods":6,"vk":75},"device":{"id":"P","name":"P"}},
                {"key":{"mods":6,"vk":"seventy"},"device":{"id":"Z","name":"Z"}},
                {"key":{"mods":6,"vk":76},"device":{"id":"Q","name":"Q"}}
            ],"osd":false}"#,
        )
        .unwrap();
        assert_eq!(c.bindings, vec![row(&[75], "P"), row(&[76], "Q")]);
        assert!(!c.osd);
    }

    #[test]
    fn bad_guard_target_keeps_the_rest() {
        let c = decode(
            br#"{"guard_out":{"default":"x","comms":{"id":"R","name":"R"},"mode":"notify"}}"#,
        )
        .unwrap();
        assert_eq!(c.guard_out.default, None);
        assert_eq!(c.guard_out.comms, pick("R"));
        assert_eq!(c.guard_out.mode, GuardMode::Notify);
    }

    #[test]
    fn norepeat_bit_removed_before_dedup() {
        let mut flagged = row(&[0x4B], "P");
        flagged.keys[0].mods |= MOD_NOREPEAT;
        let c = Config {
            bindings: vec![flagged, row(&[0x4B], "P")],
            ..Default::default()
        }
        .cleaned();
        assert_eq!(c.bindings, vec![row(&[0x4B], "P")]);
    }

    #[test]
    fn legacy_key_and_key_list_both_read() {
        let c = decode(
            br#"{"bindings":[
                {"key":{"mods":6,"vk":75},"device":{"id":"P","name":"P"}},
                {"keys":[{"mods":6,"vk":76},{"mods":6,"vk":74}],"device":{"id":"Q","name":"Q"}}
            ]}"#,
        )
        .unwrap();
        let pair = row(&[76, 74], "Q");
        assert_eq!(c.bindings, vec![row(&[75], "P"), pair.clone()]);
        let json = serde_json::to_string(&pair).unwrap();
        assert!(json.contains(r#""keys":"#), "{json}");
        assert!(!json.contains(r#""key":"#), "{json}");
    }

    #[test]
    fn cleaned_works_per_key() {
        let c = Config {
            bindings: vec![
                // Same key twice in one row, and Ctrl alone.
                row(&[75, 75, 0x11, 76], "P"),
                // 75 already belongs to P above, so only 74 stays.
                row(&[75, 74], "P"),
                // 75 on a different device is a cycle and stays.
                row(&[75], "Q"),
                // No usable key, so the whole row goes.
                row(&[0], "R"),
            ],
            ..Default::default()
        }
        .cleaned();
        assert_eq!(
            c.bindings,
            vec![row(&[75, 76], "P"), row(&[74], "P"), row(&[75], "Q")]
        );
    }
}
