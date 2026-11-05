//! Reading a rollout's fleet-wide plan: which hosts a line applies to, and
//! what a module changes on how many of them.

use std::collections::BTreeSet;

use buckle_api::{DiffKind, FleetModule, FleetPlan, FleetStep, StepOutcome, StepView};

use super::format::count;

/// What a module (or block) changes: `+create ~modify -remove` and blocked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub create: usize,
    pub modify: usize,
    pub remove: usize,
    pub blocked: usize,
}

impl From<buckle_api::ChangeCounts> for Tally {
    fn from(c: buckle_api::ChangeCounts) -> Self {
        Tally {
            create: c.create as usize,
            modify: c.modify as usize,
            remove: c.remove as usize,
            blocked: c.blocked as usize,
        }
    }
}

impl Tally {
    pub fn is_empty(&self) -> bool {
        *self == Tally::default()
    }

    /// One step's contribution; a step already in place counts for nothing.
    pub fn of_step(s: &StepView) -> Tally {
        let mut t = Tally::default();
        match (s.outcome, s.diff) {
            (StepOutcome::NoOp, _) => {}
            (StepOutcome::Blocked, _) => t.blocked = 1,
            (_, Some(DiffKind::Create)) => t.create = 1,
            (_, Some(DiffKind::Delete)) => t.remove = 1,
            _ => t.modify = 1,
        }
        t
    }

    pub fn of_steps<'a>(steps: impl IntoIterator<Item = &'a StepView>) -> Tally {
        steps
            .into_iter()
            .map(Tally::of_step)
            .fold(Tally::default(), Tally::sum)
    }

    /// A resource across hosts counts once for each kind of change its
    /// versions make (created on some hosts and modified on others is both).
    pub fn of_fleet_step(s: &FleetStep) -> Tally {
        s.variants
            .iter()
            .map(|v| Tally::of_step(&v.value))
            .fold(Tally::default(), |a, b| Tally {
                create: a.create.max(b.create),
                modify: a.modify.max(b.modify),
                remove: a.remove.max(b.remove),
                blocked: a.blocked.max(b.blocked),
            })
    }

    pub fn sum(self, o: Tally) -> Tally {
        Tally {
            create: self.create + o.create,
            modify: self.modify + o.modify,
            remove: self.remove + o.remove,
            blocked: self.blocked + o.blocked,
        }
    }
}

/// A fleet module as its collapsed heading summarises it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSummary {
    pub tally: Tally,
    /// Hosts the module changes something on, sorted.
    pub changed: Vec<String>,
    pub handlers: usize,
}

impl ModuleSummary {
    /// A module with nothing to apply anywhere is left out of the plan.
    pub fn has_changes(&self) -> bool {
        !self.tally.is_empty() || self.handlers > 0
    }
}

pub fn summarise(m: &FleetModule) -> ModuleSummary {
    let changed: BTreeSet<&String> = m
        .steps
        .iter()
        .flat_map(FleetStep::changed_hosts)
        .chain(m.handlers.iter().flat_map(|h| h.hosts.iter()))
        .collect();
    ModuleSummary {
        tally: m
            .steps
            .iter()
            .map(Tally::of_fleet_step)
            .fold(Tally::default(), Tally::sum),
        changed: changed.into_iter().cloned().collect(),
        handlers: m.handlers.len(),
    }
}

/// What a rollout changes across its fleet: each resource counted once
/// however many hosts it is on ([`FleetPlan::changes`]).
pub fn fleet_tally(f: &FleetPlan) -> Tally {
    f.changes().into()
}

/// How many of the `total` targets under review a block changes, for
/// targets named `noun` (`("host", "hosts")`): `all 12 hosts changed`, or
/// `4 changed · 8 unchanged`.
pub fn targets_changed(total: usize, changed: usize, (one, many): (&str, &str)) -> String {
    match (total, changed) {
        (1, 1) => format!("1 {one} changed"),
        (t, c) if c >= t => format!("all {t} {many} changed"),
        (t, c) => format!("{c} changed · {} unchanged", t - c),
    }
}

/// A rollout's phases in order, each with its hosts: a plan across phases
/// names the phases a line covers rather than its hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phases(pub Vec<(String, Vec<String>)>);

impl Phases {
    /// Each phase's hosts among `all` (the hosts under review), empty
    /// phases left out.
    fn within<'a>(&'a self, all: &[String]) -> Vec<(&'a str, Vec<&'a String>)> {
        self.0
            .iter()
            .map(|(n, h)| {
                (
                    n.as_str(),
                    h.iter().filter(|h| all.contains(h)).collect::<Vec<_>>(),
                )
            })
            .filter(|(_, h)| !h.is_empty())
            .collect()
    }
}

/// Whether `hosts` covers every one of `all`, and how to say who it covers:
/// `all 5 hosts`; across phases, its [`phase_label`]; else a short list
/// of names, or `3 of 5 hosts`.
pub fn scope_label(all: &[String], hosts: &[String], phases: &Phases) -> (bool, String) {
    let fleet_wide = !all.is_empty() && all.iter().all(|h| hosts.contains(h));
    if fleet_wide {
        return match all.len() {
            1 => (true, all[0].clone()),
            n => (true, format!("all {n} hosts")),
        };
    }
    if let Some(label) = phase_label(all, hosts, phases) {
        return (false, label);
    }
    let label = match (all.len(), hosts.len()) {
        (_, 1 | 2) => hosts.join(", "),
        (n, k) => format!("{k} of {}", count(n, "host", "hosts")),
    };
    (false, label)
}

/// Who part of `all` is, said by phase where the rollout has more than one
/// under review: the up to three whole phases it covers (`home, cloud`), or
/// more than two hosts of one phase (`4 of 12 in cloud`).
pub fn phase_label(all: &[String], hosts: &[String], phases: &Phases) -> Option<String> {
    let within = phases.within(all);
    let touched: Vec<&(&str, Vec<&String>)> = within
        .iter()
        .filter(|(_, h)| h.iter().any(|x| hosts.contains(x)))
        .collect();
    let placed = hosts
        .iter()
        .all(|x| within.iter().any(|(_, h)| h.contains(&x)));
    if within.len() < 2 || !placed {
        return None;
    }
    let whole = touched
        .iter()
        .all(|(_, h)| h.iter().all(|x| hosts.contains(x)));
    match (whole, touched.as_slice()) {
        (true, t) if !t.is_empty() && t.len() <= 3 => {
            Some(t.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))
        }
        (false, [(name, h)]) if hosts.len() > 2 => {
            Some(format!("{} of {} in {name}", hosts.len(), h.len()))
        }
        _ => None,
    }
}

/// The hosts a line covers, by phase where there are phases:
/// `home: pi-2 · cloud: oci-1, oci-2`.
pub fn scope_title(hosts: &[String], phases: &Phases) -> String {
    let mut parts: Vec<String> = phases
        .0
        .iter()
        .filter_map(|(n, h)| {
            let here: Vec<&str> = h
                .iter()
                .filter(|x| hosts.contains(x))
                .map(String::as_str)
                .collect();
            (!here.is_empty()).then(|| format!("{n}: {}", here.join(", ")))
        })
        .collect();
    let elsewhere: Vec<&str> = hosts
        .iter()
        .filter(|x| !phases.0.iter().any(|(_, h)| h.contains(x)))
        .map(String::as_str)
        .collect();
    if !elsewhere.is_empty() {
        parts.push(elsewhere.join(", "));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn step(outcome: StepOutcome, diff: Option<DiffKind>) -> StepView {
        StepView {
            outcome,
            diff,
            ..StepView::default()
        }
    }

    fn fleet_step(variants: &[(&[&str], StepOutcome, Option<DiffKind>)]) -> FleetStep {
        FleetStep {
            variants: variants
                .iter()
                .map(|(hosts, o, d)| Shared {
                    hosts: names(hosts),
                    value: step(*o, *d),
                })
                .collect(),
            ..FleetStep::default()
        }
    }

    use StepOutcome::*;
    use buckle_api::{HandlerView, Shared};

    #[test]
    fn a_step_tallies_by_what_it_does() {
        for (outcome, diff, want) in [
            (
                Change,
                Some(DiffKind::Create),
                Tally {
                    create: 1,
                    ..Tally::default()
                },
            ),
            (
                Change,
                Some(DiffKind::Update),
                Tally {
                    modify: 1,
                    ..Tally::default()
                },
            ),
            (
                Change,
                None,
                Tally {
                    modify: 1,
                    ..Tally::default()
                },
            ),
            (
                Change,
                Some(DiffKind::Delete),
                Tally {
                    remove: 1,
                    ..Tally::default()
                },
            ),
            (
                Blocked,
                None,
                Tally {
                    blocked: 1,
                    ..Tally::default()
                },
            ),
            (NoOp, None, Tally::default()),
        ] {
            assert_eq!(
                Tally::of_step(&step(outcome, diff)),
                want,
                "{outcome:?} {diff:?}"
            );
        }
    }

    #[test]
    fn a_module_counts_each_resource_once_per_kind_and_the_hosts_it_changes() {
        let m = FleetModule {
            module: "nomad".into(),
            hosts: names(&["a", "b", "c", "d"]),
            steps: vec![
                fleet_step(&[
                    (&["a"], Change, Some(DiffKind::Create)),
                    (&["b"], Change, Some(DiffKind::Update)),
                    (&["c"], Change, Some(DiffKind::Update)),
                ]),
                fleet_step(&[(&["b"], Change, Some(DiffKind::Delete))]),
                FleetStep {
                    unchanged: names(&["a", "b", "c", "d"]),
                    ..FleetStep::default()
                },
            ],
            ..FleetModule::default()
        };
        let s = summarise(&m);
        assert_eq!(
            s.tally,
            Tally {
                create: 1,
                modify: 1,
                remove: 1,
                blocked: 0
            }
        );
        assert_eq!(s.changed, names(&["a", "b", "c"]));
        assert!(s.has_changes());
    }

    #[test]
    fn a_module_changes_nothing_unless_a_step_or_handler_does() {
        let mut m = FleetModule {
            steps: vec![FleetStep {
                unchanged: names(&["a"]),
                ..FleetStep::default()
            }],
            ..FleetModule::default()
        };
        assert!(!summarise(&m).has_changes());
        m.handlers = vec![Shared {
            hosts: names(&["a"]),
            value: HandlerView::default(),
        }];
        assert!(summarise(&m).has_changes());
    }

    #[test]
    fn targets_changed_reads_as_all_or_changed_against_unchanged() {
        let hosts = ("host", "hosts");
        assert_eq!(targets_changed(5, 5, hosts), "all 5 hosts changed");
        assert_eq!(targets_changed(5, 2, hosts), "2 changed · 3 unchanged");
        assert_eq!(targets_changed(1, 1, hosts), "1 host changed");
    }

    #[test]
    fn scope_reads_as_all_hosts_or_names_the_subset() {
        let all = names(&["a", "b", "c", "d"]);
        let none = Phases::default();
        assert_eq!(scope_label(&all, &all, &none), (true, "all 4 hosts".into()));
        assert_eq!(
            scope_label(&all, &names(&["a", "c"]), &none),
            (false, "a, c".into())
        );
        assert_eq!(
            scope_label(&all, &names(&["a", "b", "c"]), &none),
            (false, "3 of 4 hosts".into())
        );
        assert!(scope_label(&names(&["a"]), &names(&["a"]), &none).0);
    }

    fn phases() -> Phases {
        Phases(vec![
            ("canary".into(), names(&["a"])),
            ("home".into(), names(&["b", "c", "d"])),
            ("cloud".into(), names(&["e", "f", "g", "h"])),
            ("lab".into(), names(&["i"])),
            // Not under review: no plan yet.
            ("later".into(), names(&["z"])),
        ])
    }

    #[test]
    fn across_phases_scope_names_the_phases_it_covers() {
        let all = names(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
        for (hosts, want) in [
            (&["a"][..], "canary"),
            (&["b", "c", "d"], "home"),
            (&["a", "e", "f", "g", "h"], "canary, cloud"),
            (
                &["a", "b", "c", "d", "e", "f", "g", "h"],
                "canary, home, cloud",
            ),
            (&["e", "f", "g"], "3 of 4 in cloud"),
            (&["e", "f"], "e, f"),
            (&["a", "b", "e"], "3 of 9 hosts"),
        ] {
            assert_eq!(
                scope_label(&all, &names(hosts), &phases()),
                (false, want.into()),
                "{hosts:?}"
            );
        }
        assert_eq!(scope_label(&all, &all, &phases()).1, "all 9 hosts");
    }

    #[test]
    fn scope_title_groups_hosts_by_phase() {
        assert_eq!(
            scope_title(&names(&["a", "e", "f", "x"]), &phases()),
            "canary: a · cloud: e, f · x"
        );
        assert_eq!(scope_title(&names(&["a", "b"]), &Phases::default()), "a, b");
    }
}
