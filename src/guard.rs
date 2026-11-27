//! Decides when the default-device guard reverts, notifies or backs off.

use crate::audio::{Current, RoleIds};
use crate::config::{Config, DeviceRef, GuardMode, GuardSide};
use crate::devices::{DeviceInfo, resolve};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// More than `REVERTS_PER_SPAN` reverts inside `REVERT_SPAN` means something keeps undoing them.
const REVERT_SPAN: Duration = Duration::from_secs(60);
const REVERTS_PER_SPAN: usize = 5;
pub const BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Side {
    Out,
    In,
}

/// Which role groups differ from the guarded target.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Drift {
    pub default_roles: bool,
    pub comms_role: bool,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Revert {
        out: Option<Drift>,
        inp: Option<Drift>,
    },
    Notify {
        out: Option<Drift>,
        inp: Option<Drift>,
    },
    GiveUp,
}

#[derive(Default)]
pub struct Guard {
    reverts: VecDeque<Instant>,
    backoff_until: Option<Instant>,
    snoozed_out: Option<Instant>,
    snoozed_in: Option<Instant>,
    last_notified: Option<Current>,
}

/// An unresolved or unplugged target is not drift: reverting to it would fail anyway.
fn present_id<'a>(r: Option<&DeviceRef>, list: &'a [DeviceInfo]) -> Option<&'a str> {
    resolve(r?, list)
        .filter(|d| d.present)
        .map(|d| d.id.as_str())
}

/// Present target ids for the default roles and the comms role; comms falls back to default.
pub fn targets<'a>(side: &GuardSide, list: &'a [DeviceInfo]) -> (Option<&'a str>, Option<&'a str>) {
    (
        present_id(side.default.as_ref(), list),
        present_id(side.comms.as_ref().or(side.default.as_ref()), list),
    )
}

/// "Keep" adopts what Windows uses now; a comms device equal to default is stored as "same".
pub fn keep(side: &mut GuardSide, roles: &RoleIds, list: &[DeviceInfo]) {
    let to_ref = |id: &str| DeviceRef {
        id: id.to_string(),
        name: list
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_default(),
    };
    side.default = roles[0].as_deref().map(to_ref);
    side.comms = roles[2]
        .as_deref()
        .filter(|&c| Some(c) != roles[0].as_deref())
        .map(to_ref);
}

fn drift(side: &GuardSide, roles: &RoleIds, list: &[DeviceInfo]) -> Option<Drift> {
    let differs = |want: Option<&str>, role: &Option<String>| {
        want.is_some_and(|id| role.as_deref() != Some(id))
    };
    let (def, comms) = targets(side, list);
    let d = Drift {
        default_roles: differs(def, &roles[0]) || differs(def, &roles[1]),
        comms_role: differs(comms, &roles[2]),
    };
    (d.default_roles || d.comms_role).then_some(d)
}

impl Guard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(
        &mut self,
        cfg: &Config,
        cur: &Current,
        outs: &[DeviceInfo],
        ins: &[DeviceInfo],
        now: Instant,
    ) -> Action {
        if !cfg.guard_enabled || self.backoff_until.is_some_and(|until| now < until) {
            return Action::None;
        }
        let check = |side: &GuardSide, snoozed: Option<Instant>, roles, list| {
            if side.mode == GuardMode::Off || snoozed.is_some_and(|t| now < t) {
                return None;
            }
            drift(side, roles, list).map(|d| (side.mode, d))
        };
        let out = check(&cfg.guard_out, self.snoozed_out, &cur.render, outs);
        let inp = check(&cfg.guard_in, self.snoozed_in, &cur.capture, ins);
        let pick = |s: Option<(GuardMode, Drift)>, m| s.filter(|x| x.0 == m).map(|x| x.1);

        // A Notify side waits while the other side reverts; it is seen again next event.
        let (r_out, r_inp) = (
            pick(out, GuardMode::AutoRevert),
            pick(inp, GuardMode::AutoRevert),
        );
        if r_out.is_some() || r_inp.is_some() {
            return self.revert(r_out, r_inp, now);
        }
        let (n_out, n_inp) = (pick(out, GuardMode::Notify), pick(inp, GuardMode::Notify));
        if n_out.is_none() && n_inp.is_none() {
            self.last_notified = None;
            return Action::None;
        }
        // Device events repeat for one drift; only a new role state warrants a popup.
        if self.last_notified.as_ref() == Some(cur) {
            return Action::None;
        }
        self.last_notified = Some(cur.clone());
        Action::Notify {
            out: n_out,
            inp: n_inp,
        }
    }

    /// If another program keeps switching back, reverting forever would just ping-pong.
    fn revert(&mut self, out: Option<Drift>, inp: Option<Drift>, now: Instant) -> Action {
        while let Some(&oldest) = self.reverts.front() {
            if now.duration_since(oldest) <= REVERT_SPAN {
                break;
            }
            self.reverts.pop_front();
        }
        if self.reverts.len() < REVERTS_PER_SPAN {
            self.reverts.push_back(now);
            return Action::Revert { out, inp };
        }
        self.reverts.clear();
        self.backoff_until = Some(now + BACKOFF);
        Action::GiveUp
    }

    pub fn snooze(&mut self, side: Side, until: Instant) {
        match side {
            Side::Out => self.snoozed_out = Some(until),
            Side::In => self.snoozed_in = Some(until),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A target picked in settings is the user's choice: switch to it now instead of asking.
/// `None` when nothing is to be done (target unchanged, guard off, or Windows already matches).
pub fn on_new_target(
    cfg: &Config,
    old: &GuardSide,
    side: Side,
    cur: &Current,
    list: &[DeviceInfo],
) -> Option<Drift> {
    let (g, roles) = match side {
        Side::Out => (&cfg.guard_out, &cur.render),
        Side::In => (&cfg.guard_in, &cur.capture),
    };
    let changed = g.default != old.default || g.comms != old.comms;
    if !changed || !cfg.guard_enabled || g.mode == GuardMode::Off {
        return None;
    }
    drift(g, roles, list)
}

/// A shown Notify popup is stale once its side no longer needs it: guard off, mode changed,
/// or Windows already matches the target (a new target, or someone switched back).
pub fn prompt_stale(cfg: &Config, side: Side, cur: &Current, list: &[DeviceInfo]) -> bool {
    let (g, roles) = match side {
        Side::Out => (&cfg.guard_out, &cur.render),
        Side::In => (&cfg.guard_in, &cur.capture),
    };
    !cfg.guard_enabled || g.mode != GuardMode::Notify || drift(g, roles, list).is_none()
}

/// A hotkey switch is the user's new choice, so the guard follows it.
/// An Off guard keeps its empty target so the first pick in settings can still turn on Notify.
pub fn after_switch(cfg: &mut Config, dev: &DeviceRef) {
    let side = &mut cfg.guard_out;
    if side.mode == GuardMode::Off {
        return;
    }
    if cfg.also_comms {
        side.comms = None;
    } else if side.comms.is_none() {
        // Comms followed default; the switch left the comms role where it was.
        side.comms = side.default.clone();
    }
    side.default = Some(dev.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> Option<String> {
        Some(name.to_owned())
    }
    fn dref(id: &str) -> DeviceRef {
        DeviceRef {
            id: id.into(),
            name: id.into(),
        }
    }
    fn info(id: &str, present: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            present,
        }
    }
    fn outs() -> Vec<DeviceInfo> {
        vec![info("S", true), info("H", true), info("X", true)]
    }
    fn ins() -> Vec<DeviceInfo> {
        vec![info("M", true), info("Y", true)]
    }
    fn conf(out_mode: GuardMode) -> Config {
        Config {
            guard_out: GuardSide {
                default: Some(dref("S")),
                comms: None,
                mode: out_mode,
            },
            guard_in: GuardSide {
                default: Some(dref("M")),
                comms: None,
                mode: GuardMode::Off,
            },
            ..Default::default()
        }
    }
    fn steady() -> Current {
        Current {
            render: [id("S"), id("S"), id("S")],
            capture: [id("M"), id("M"), id("M")],
        }
    }
    fn console_drift() -> Current {
        let mut c = steady();
        c.render[0] = id("X");
        c
    }
    fn plus(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }
    const CONSOLE: Drift = Drift {
        default_roles: true,
        comms_role: false,
    };
    const COMMS: Drift = Drift {
        default_roles: false,
        comms_role: true,
    };

    fn run(g: &mut Guard, c: &Config, cur: &Current, now: Instant) -> Action {
        g.evaluate(c, cur, &outs(), &ins(), now)
    }

    #[test]
    fn master_off_is_none() {
        let mut c = conf(GuardMode::AutoRevert);
        c.guard_enabled = false;
        let a = run(&mut Guard::new(), &c, &console_drift(), Instant::now());
        assert_eq!(a, Action::None);
    }

    #[test]
    fn mode_off_is_none() {
        let c = conf(GuardMode::Off);
        let mut cur = console_drift();
        cur.capture[0] = id("Y");
        assert_eq!(
            run(&mut Guard::new(), &c, &cur, Instant::now()),
            Action::None
        );
    }

    #[test]
    fn no_drift_is_none() {
        let c = conf(GuardMode::AutoRevert);
        assert_eq!(
            run(&mut Guard::new(), &c, &steady(), Instant::now()),
            Action::None
        );
    }

    #[test]
    fn console_drift_detected() {
        let c = conf(GuardMode::AutoRevert);
        assert_eq!(
            run(&mut Guard::new(), &c, &console_drift(), Instant::now()),
            Action::Revert {
                out: Some(CONSOLE),
                inp: None
            }
        );
    }

    #[test]
    fn comms_checked_against_comms_target() {
        let mut c = conf(GuardMode::AutoRevert);
        c.guard_out.comms = Some(dref("H"));
        let mut cur = steady();
        cur.render[2] = id("H");
        let mut g = Guard::new();
        assert_eq!(run(&mut g, &c, &cur, Instant::now()), Action::None);
        assert_eq!(
            run(&mut g, &c, &steady(), Instant::now()),
            Action::Revert {
                out: Some(COMMS),
                inp: None
            }
        );
    }

    #[test]
    fn comms_none_means_same_as_default() {
        let c = conf(GuardMode::AutoRevert);
        let mut cur = steady();
        cur.render[2] = id("H");
        assert_eq!(
            run(&mut Guard::new(), &c, &cur, Instant::now()),
            Action::Revert {
                out: Some(COMMS),
                inp: None
            }
        );
    }

    #[test]
    fn absent_target_is_not_drift() {
        let c = conf(GuardMode::AutoRevert);
        let mut g = Guard::new();
        let now = Instant::now();
        let unplugged = [info("S", false), info("X", true)];
        let a = g.evaluate(&c, &console_drift(), &unplugged, &ins(), now);
        assert_eq!(a, Action::None);
        let unknown = [info("X", true)];
        let a = g.evaluate(&c, &console_drift(), &unknown, &ins(), now);
        assert_eq!(a, Action::None);
    }

    #[test]
    fn gives_up_after_five() {
        let c = conf(GuardMode::AutoRevert);
        let mut g = Guard::new();
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(
                run(&mut g, &c, &console_drift(), plus(t0, i * 10)),
                Action::Revert {
                    out: Some(CONSOLE),
                    inp: None
                }
            );
        }
        let cur = console_drift();
        assert_eq!(run(&mut g, &c, &cur, plus(t0, 50)), Action::GiveUp);
        assert_eq!(run(&mut g, &c, &cur, plus(t0, 51)), Action::None);
        assert_eq!(run(&mut g, &c, &cur, plus(t0, 349)), Action::None);
        assert!(matches!(
            run(&mut g, &c, &cur, plus(t0, 350)),
            Action::Revert { .. }
        ));
    }

    #[test]
    fn notify_once_per_drift() {
        let c = conf(GuardMode::Notify);
        let mut g = Guard::new();
        let now = Instant::now();
        let n = Action::Notify {
            out: Some(CONSOLE),
            inp: None,
        };
        assert_eq!(run(&mut g, &c, &console_drift(), now), n);
        assert_eq!(run(&mut g, &c, &console_drift(), now), Action::None);
        let mut other = console_drift();
        other.render[0] = id("H");
        assert_eq!(run(&mut g, &c, &other, now), n);
    }

    #[test]
    fn snoozed_side_ignored_until_time() {
        let c = conf(GuardMode::AutoRevert);
        let mut g = Guard::new();
        let t0 = Instant::now();
        g.snooze(Side::Out, plus(t0, 60));
        assert_eq!(
            run(&mut g, &c, &console_drift(), plus(t0, 59)),
            Action::None
        );
        assert!(matches!(
            run(&mut g, &c, &console_drift(), plus(t0, 60)),
            Action::Revert { .. }
        ));
    }

    #[test]
    fn mixed_modes_revert_wins() {
        let mut c = conf(GuardMode::Notify);
        c.guard_in.mode = GuardMode::AutoRevert;
        let mut cur = console_drift();
        cur.capture[2] = id("Y");
        let mut g = Guard::new();
        let now = Instant::now();
        assert_eq!(
            run(&mut g, &c, &cur, now),
            Action::Revert {
                out: None,
                inp: Some(COMMS)
            }
        );
        cur.capture[2] = id("M");
        assert_eq!(
            run(&mut g, &c, &cur, now),
            Action::Notify {
                out: Some(CONSOLE),
                inp: None
            }
        );
    }

    #[test]
    fn after_switch_moves_out_target_and_comms() {
        let mut c = conf(GuardMode::Notify);
        c.guard_out.comms = Some(dref("H"));
        after_switch(&mut c, &dref("X"));
        assert_eq!(c.guard_out.default, Some(dref("X")));
        assert_eq!(c.guard_out.comms, None);
        assert_eq!(c.guard_out.mode, GuardMode::Notify);
    }

    #[test]
    fn after_switch_keeps_comms_when_also_comms_false() {
        let mut c = conf(GuardMode::Notify);
        c.also_comms = false;
        c.guard_out.comms = Some(dref("H"));
        after_switch(&mut c, &dref("X"));
        assert_eq!(c.guard_out.default, Some(dref("X")));
        assert_eq!(c.guard_out.comms, Some(dref("H")));
    }

    #[test]
    fn after_switch_pins_old_comms_when_also_comms_false() {
        let mut c = conf(GuardMode::Notify);
        c.also_comms = false;
        after_switch(&mut c, &dref("X"));
        assert_eq!(c.guard_out.default, Some(dref("X")));
        assert_eq!(c.guard_out.comms, Some(dref("S")));
    }

    #[test]
    fn after_switch_leaves_off_guard_alone() {
        let mut c = conf(GuardMode::Off);
        let before = c.guard_out.clone();
        after_switch(&mut c, &dref("X"));
        assert_eq!(c.guard_out, before);
    }

    #[test]
    fn targets_comms_falls_back_to_default() {
        let list = outs();
        let mut side = conf(GuardMode::Notify).guard_out;
        assert_eq!(targets(&side, &list), (Some("S"), Some("S")));
        side.comms = Some(dref("H"));
        assert_eq!(targets(&side, &list), (Some("S"), Some("H")));
        let gone = [info("S", false), info("H", true)];
        assert_eq!(targets(&side, &gone), (None, Some("H")));
    }

    #[test]
    fn keep_sets_from_current() {
        let mut side = conf(GuardMode::Notify).guard_out;
        keep(&mut side, &[id("H"), id("H"), id("X")], &outs());
        assert_eq!(side.default, Some(dref("H")));
        assert_eq!(side.comms, Some(dref("X")));
        assert_eq!(side.mode, GuardMode::Notify);
    }

    #[test]
    fn keep_clears_comms_when_equal() {
        let mut side = conf(GuardMode::Notify).guard_out;
        side.comms = Some(dref("X"));
        keep(&mut side, &[id("H"), id("H"), id("H")], &outs());
        assert_eq!(side.default, Some(dref("H")));
        assert_eq!(side.comms, None);
    }

    #[test]
    fn prompt_stale_when_drift_gone_or_guard_changed() {
        let mut c = conf(GuardMode::Notify);
        c.guard_enabled = true;
        let cur = console_drift();
        assert!(!prompt_stale(&c, Side::Out, &cur, &outs()));
        // Popup was for S while Windows sits on X; the user then picks X as the target.
        let on_x = Current {
            render: [id("X"), id("X"), id("X")],
            ..steady()
        };
        assert!(!prompt_stale(&c, Side::Out, &on_x, &outs()));
        let mut fixed = c.clone();
        fixed.guard_out.default = Some(dref("X"));
        assert!(prompt_stale(&fixed, Side::Out, &on_x, &outs()));
        assert!(prompt_stale(&c, Side::Out, &steady(), &outs()));
        let mut auto = c.clone();
        auto.guard_out.mode = GuardMode::AutoRevert;
        assert!(prompt_stale(&auto, Side::Out, &cur, &outs()));
        let mut off = c.clone();
        off.guard_enabled = false;
        assert!(prompt_stale(&off, Side::Out, &cur, &outs()));
        let mut none = c;
        none.guard_out.default = None;
        assert!(prompt_stale(&none, Side::Out, &cur, &outs()));
    }

    #[test]
    fn new_target_switches_without_asking() {
        let mut c = conf(GuardMode::Notify);
        c.guard_enabled = true;
        let old = c.guard_out.clone();
        // Windows on S; the user picks X as the target.
        c.guard_out.default = Some(dref("X"));
        let d = on_new_target(&c, &old, Side::Out, &steady(), &outs());
        assert_eq!(
            d,
            Some(Drift {
                default_roles: true,
                comms_role: true
            })
        );
        // Unchanged target, guard off, mode Off, or already on X: nothing to do.
        assert_eq!(
            on_new_target(&c, &c.guard_out, Side::Out, &steady(), &outs()),
            None
        );
        let mut off = c.clone();
        off.guard_enabled = false;
        assert_eq!(
            on_new_target(&off, &old, Side::Out, &steady(), &outs()),
            None
        );
        let mut m = c.clone();
        m.guard_out.mode = GuardMode::Off;
        assert_eq!(on_new_target(&m, &old, Side::Out, &steady(), &outs()), None);
        let on_x = Current {
            render: [id("X"), id("X"), id("X")],
            ..steady()
        };
        assert_eq!(on_new_target(&c, &old, Side::Out, &on_x, &outs()), None);
    }
}
