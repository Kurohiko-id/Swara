//! Pure device lookup and hotkey cycle order, kept apart from COM so it can be tested.

use crate::config::{Binding, DeviceRef, Hotkey};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub present: bool,
}

/// A present id match wins. Next comes a present device of the same name, because endpoint ids
/// change when a device moves to another port and the old id stays listed as absent. An absent
/// id match is last, so the caller can still report that exact device as missing.
pub fn resolve<'a>(r: &DeviceRef, list: &'a [DeviceInfo]) -> Option<&'a DeviceInfo> {
    match list.iter().find(|d| d.id == r.id) {
        Some(d) if d.present => Some(d),
        absent => list
            .iter()
            .find(|d| d.present && d.name == r.name)
            .or(absent),
    }
}

pub fn display_name(info_name: &str, id: &str, nick: &BTreeMap<String, String>) -> String {
    nick.get(id)
        .cloned()
        .unwrap_or_else(|| info_name.to_string())
}

pub fn targets_for(key: Hotkey, bindings: &[Binding]) -> Vec<&DeviceRef> {
    bindings
        .iter()
        .filter(|b| b.keys.contains(&key))
        .map(|b| &b.device)
        .collect()
}

/// A single target is returned even when already default, so its key re-applies it.
pub fn next_target(
    targets: &[&DeviceRef],
    list: &[DeviceInfo],
    current: Option<&str>,
) -> Option<DeviceInfo> {
    let mut live: Vec<&DeviceInfo> = Vec::new();
    for d in targets.iter().filter_map(|r| resolve(r, list)) {
        // Two rows can land on one device via the name fallback; a repeat would stall the cycle.
        if d.present && !live.iter().any(|x| x.id == d.id) {
            live.push(d);
        }
    }
    let next = match live.iter().position(|d| Some(d.id.as_str()) == current) {
        Some(i) => (i + 1) % live.len(),
        None => 0,
    };
    live.get(next).map(|d| (*d).clone())
}

pub fn cycle_label(i: usize, n: usize) -> Option<(usize, usize)> {
    (n > 1).then_some((i + 1, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str, present: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            present,
        }
    }

    fn r(id: &str, name: &str) -> DeviceRef {
        DeviceRef {
            id: id.into(),
            name: name.into(),
        }
    }

    fn abc() -> (Vec<DeviceRef>, Vec<DeviceInfo>) {
        (
            vec![r("a", "A"), r("b", "B"), r("c", "C")],
            vec![
                dev("a", "A", true),
                dev("b", "B", true),
                dev("c", "C", true),
            ],
        )
    }

    fn next(refs: &[DeviceRef], list: &[DeviceInfo], cur: Option<&str>) -> Option<String> {
        let t: Vec<&DeviceRef> = refs.iter().collect();
        next_target(&t, list, cur).map(|d| d.id)
    }

    #[test]
    fn resolve_by_id() {
        let list = [dev("x", "Speakers", true), dev("a", "Headset", true)];
        assert_eq!(resolve(&r("a", "Speakers"), &list).unwrap().id, "a");
    }

    #[test]
    fn resolve_prefers_present_same_name_over_absent_id() {
        // Replugged into another port: the old id stays listed as absent.
        let list = [dev("a", "Headset", false), dev("n", "Headset", true)];
        assert_eq!(resolve(&r("a", "Headset"), &list).unwrap().id, "n");
        // A present id match still beats a same-named twin.
        let twins = [dev("n", "Headset", true), dev("a", "Headset", true)];
        assert_eq!(resolve(&r("a", "Headset"), &twins).unwrap().id, "a");
    }

    #[test]
    fn resolve_keeps_absent_id_when_nothing_else_matches() {
        let list = [dev("a", "Headset", false), dev("x", "Speakers", true)];
        let hit = resolve(&r("a", "Headset"), &list).unwrap();
        assert_eq!(hit.id, "a");
        assert!(!hit.present);
    }

    #[test]
    fn replugged_device_stays_in_the_cycle() {
        let refs = [r("a", "Headset"), r("b", "Speakers")];
        let list = [
            dev("a", "Headset", false),
            dev("a2", "Headset", true),
            dev("b", "Speakers", true),
        ];
        assert_eq!(next(&refs, &list, Some("b")).as_deref(), Some("a2"));
        assert_eq!(next(&refs, &list, Some("a2")).as_deref(), Some("b"));
    }

    #[test]
    fn resolve_falls_back_to_name() {
        let list = [
            dev("old", "Other", true),
            dev("n1", "Headset", false),
            dev("n2", "Headset", true),
        ];
        assert_eq!(resolve(&r("gone", "Headset"), &list).unwrap().id, "n2");
        assert!(resolve(&r("gone", "Nope"), &list).is_none());
    }

    #[test]
    fn display_prefers_nickname() {
        let mut nick = BTreeMap::new();
        nick.insert("a".to_string(), "Desk".to_string());
        assert_eq!(display_name("Speakers", "a", &nick), "Desk");
        assert_eq!(display_name("Speakers", "b", &nick), "Speakers");
    }

    #[test]
    fn targets_keep_row_order() {
        let k1 = Hotkey { mods: 2, vk: 0x31 };
        let k2 = Hotkey { mods: 2, vk: 0x32 };
        let bind = |keys: &[Hotkey], id| Binding {
            keys: keys.to_vec(),
            device: r(id, id),
        };
        // Row "a" answers to both keys.
        let bindings = [
            bind(&[k1], "c"),
            bind(&[k2], "x"),
            bind(&[k2, k1], "a"),
            bind(&[k1], "b"),
        ];
        let ids = |k| -> Vec<String> {
            targets_for(k, &bindings)
                .iter()
                .map(|d| d.id.clone())
                .collect()
        };
        assert_eq!(ids(k1), ["c", "a", "b"]);
        assert_eq!(ids(k2), ["x", "a"]);
    }

    #[test]
    fn next_after_current() {
        let (refs, list) = abc();
        assert_eq!(next(&refs, &list, Some("b")).as_deref(), Some("c"));
    }

    #[test]
    fn next_wraps() {
        let (refs, list) = abc();
        assert_eq!(next(&refs, &list, Some("c")).as_deref(), Some("a"));
    }

    #[test]
    fn next_when_current_not_in_list_is_first() {
        let (refs, list) = abc();
        assert_eq!(next(&refs, &list, Some("z")).as_deref(), Some("a"));
        assert_eq!(next(&refs, &list, None).as_deref(), Some("a"));
    }

    #[test]
    fn next_skips_absent() {
        let (refs, mut list) = abc();
        list[1].present = false;
        assert_eq!(next(&refs, &list, Some("a")).as_deref(), Some("c"));
    }

    #[test]
    fn next_target_all_absent_is_none() {
        let (refs, mut list) = abc();
        list.iter_mut().for_each(|d| d.present = false);
        assert_eq!(next(&refs, &list, Some("a")), None);
        assert_eq!(next(&[], &list, None), None);
    }

    #[test]
    fn next_dedupes_refs_resolving_to_one_device() {
        // "gone" falls back by name onto "a", so the group is really [a, b].
        let refs = [r("a", "A"), r("gone", "A"), r("b", "B")];
        let list = [dev("a", "A", true), dev("b", "B", true)];
        assert_eq!(next(&refs, &list, Some("a")).as_deref(), Some("b"));
        assert_eq!(next(&refs, &list, Some("b")).as_deref(), Some("a"));
    }

    #[test]
    fn single_target_returns_it_even_if_current() {
        let (refs, list) = abc();
        assert_eq!(next(&refs[..1], &list, Some("a")).as_deref(), Some("a"));
    }

    #[test]
    fn cycle_label_only_for_groups() {
        assert_eq!(cycle_label(0, 3), Some((1, 3)));
        assert_eq!(cycle_label(0, 1), None);
    }
}
