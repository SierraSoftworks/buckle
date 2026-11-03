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

/// What a rollout changes across its fleet: each module's tally and the
/// removals that run, each resource counted once however many hosts it is on.
pub fn fleet_tally(f: &FleetPlan) -> Tally {
    f.modules.iter().map(|m| summarise(m).tally).fold(
        Tally {
            remove: f.removals.iter().filter(|r| r.value.cleanup).count(),
            ..Tally::default()
        },
        Tally::sum,
    )
}

/// How many of the `total` hosts under review a block changes:
/// `all 12 hosts changed`, or `4 changed · 8 unchanged`.
pub fn hosts_changed(total: usize, changed: usize) -> String {
    match (total, changed) {
        (1, 1) => "1 host changed".into(),
        (t, c) if c >= t => format!("all {t} hosts changed"),
        (t, c) => format!("{c} changed · {} unchanged", t - c),
    }
}

/// Whether `hosts` covers every one of `all`, and how to say who it covers:
/// `all 5 hosts`, a short list of names, or `3 of 5 hosts`.
pub fn scope_label(all: &[String], hosts: &[String]) -> (bool, String) {
    let fleet_wide = !all.is_empty() && all.iter().all(|h| hosts.contains(h));
    let label = match (fleet_wide, all.len(), hosts.len()) {
        (true, 1, _) => all[0].clone(),
        (true, n, _) => format!("all {n} hosts"),
        (false, _, 1 | 2) => hosts.join(", "),
        (false, n, k) => format!("{k} of {}", count(n, "host", "hosts")),
    };
    (fleet_wide, label)
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
    fn hosts_changed_reads_as_all_or_changed_against_unchanged() {
        assert_eq!(hosts_changed(5, 5), "all 5 hosts changed");
        assert_eq!(hosts_changed(5, 2), "2 changed · 3 unchanged");
        assert_eq!(hosts_changed(1, 1), "1 host changed");
    }

    #[test]
    fn scope_reads_as_all_hosts_or_names_the_subset() {
        let all = names(&["a", "b", "c", "d"]);
        assert_eq!(scope_label(&all, &all), (true, "all 4 hosts".into()));
        assert_eq!(
            scope_label(&all, &names(&["a", "c"])),
            (false, "a, c".into())
        );
        assert_eq!(
            scope_label(&all, &names(&["a", "b", "c"])),
            (false, "3 of 4 hosts".into())
        );
        assert!(scope_label(&names(&["a"]), &names(&["a"])).0);
    }
}
