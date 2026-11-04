//! Reading a plan for review (design §11).
//!
//! `PlanGroup.plan` is the server's typed [`PlanView`] of the engine's
//! redacted plan, so what is destructive, a removal or a caution is the
//! server's call; this module only counts what an approver is asked to
//! accept.

use std::collections::BTreeSet;

use buckle_api::{PlanGroup, PlanView, RolloutHost, StepOutcome};

use super::fleet_plan::Tally;

/// What one distinct plan asks an approver to accept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    pub changes: usize,
    pub unchanged: usize,
    pub blocked: usize,
    pub destructive_steps: usize,
    /// Removals that will run on the host (`cleanup`).
    pub removals: usize,
    /// Removals that only forget the resource (`leave`).
    pub forgotten: usize,
    /// Removals of kinds that can take unrelated things down.
    pub cautions: usize,
    pub deferred: usize,
    pub always_runs: usize,
}

impl Review {
    /// Destructive steps plus removals that run: the number the approval
    /// confirmation names.
    pub fn destructive(&self) -> usize {
        self.destructive_steps + self.removals
    }
}

pub fn review(plan: &PlanView) -> Review {
    let mut r = Review::default();
    for step in plan.steps() {
        match step.outcome {
            StepOutcome::Change => r.changes += 1,
            StepOutcome::Blocked => r.blocked += 1,
            StepOutcome::NoOp => r.unchanged += 1,
        }
        r.destructive_steps += usize::from(step.destructive);
        r.always_runs += usize::from(step.always_runs && step.outcome != StepOutcome::NoOp);
    }
    for removal in &plan.removals {
        if removal.cleanup {
            r.removals += 1;
        } else {
            r.forgotten += 1;
        }
        r.cautions += usize::from(removal.caution && removal.cleanup);
    }
    r.deferred = plan.deferred.len();
    r
}

/// What a plan changes on its host: its steps by shape, and the removals
/// that run (a removal that only forgets its record changes nothing).
pub fn plan_tally(plan: &PlanView) -> Tally {
    Tally {
        remove: plan.removals.iter().filter(|r| r.cleanup).count(),
        ..Tally::default()
    }
    .sum(Tally::of_steps(plan.steps()))
}

/// What a host's plan in a rollout changes: from its plan where the server
/// holds a copy, else from the counts the host reported.
pub fn host_tally(h: &RolloutHost, plans: &[PlanGroup]) -> Option<Tally> {
    let digest = h.plan_digest.as_deref()?;
    let held = plans
        .iter()
        .find(|g| g.digest == digest)
        .and_then(|g| g.plan.as_ref());
    match (held, h.plan_changes) {
        (Some(plan), _) => Some(plan_tally(plan)),
        (None, Some(changes)) => {
            let removals = h.plan_removals.unwrap_or(0);
            Some(Tally {
                modify: changes.saturating_sub(removals) as usize,
                remove: removals as usize,
                ..Tally::default()
            })
        }
        (None, None) => None,
    }
}

/// What approving a phase binds: the distinct plans its hosts would apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalScope {
    /// Indices into the rollout's plan groups, in their order.
    pub groups: Vec<usize>,
    pub digests: Vec<String>,
    /// Hosts in the phase whose plan is one of `digests`.
    pub hosts: Vec<String>,
    /// Hosts in the phase with no plan yet (offline, still planning).
    pub without_plan: Vec<String>,
    pub destructive: usize,
    pub removals: usize,
    pub changes: usize,
}

/// Works out what an approval of a phase covers. A host belongs to a plan
/// group by its own `plan_digest` first, or by being listed in the group.
pub fn approval_scope(phase_hosts: &[&RolloutHost], plans: &[PlanGroup]) -> ApprovalScope {
    let mut scope = ApprovalScope::default();
    let mut used = BTreeSet::new();
    for host in phase_hosts {
        let group = plans.iter().position(|g| {
            host.plan_digest.as_deref() == Some(g.digest.as_str())
                || (host.plan_digest.is_none() && g.hosts.contains(&host.host))
        });
        match group {
            Some(i) => {
                used.insert(i);
                scope.hosts.push(host.host.clone());
            }
            None => scope.without_plan.push(host.host.clone()),
        }
    }
    for i in used {
        let g = &plans[i];
        scope.groups.push(i);
        scope.digests.push(g.digest.clone());
        match &g.plan {
            Some(plan) => {
                let r = review(plan);
                scope.destructive += r.destructive();
                scope.removals += r.removals;
                scope.changes += r.changes;
            }
            None => {
                scope.removals += g.removals as usize;
                scope.changes += g.changes as usize;
                scope.destructive += if g.destructive {
                    (g.removals as usize).max(1)
                } else {
                    0
                };
            }
        }
    }
    scope.hosts.sort();
    scope.without_plan.sort();
    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{DiffKind, HostRolloutState, ModuleNote, ModuleView, RemovalView, StepView};
    use chrono::Utc;

    fn step(kind: &str, key: &str, outcome: StepOutcome, diff: Option<DiffKind>) -> StepView {
        StepView {
            kind: kind.into(),
            key: key.into(),
            outcome,
            diff,
            destructive: diff == Some(DiffKind::Delete),
            ..StepView::default()
        }
    }

    fn removal(key: &str, cleanup: bool, caution: bool) -> RemovalView {
        RemovalView {
            module: "grey".into(),
            kind: "apt.package".into(),
            key: key.into(),
            summary: format!("remove {key}"),
            cleanup,
            caution,
            introduced_in: None,
        }
    }

    fn sample() -> PlanView {
        PlanView {
            revision: "7c2e1f0".into(),
            host: "pi-1".into(),
            digest: "5b1c9e02".into(),
            modules: vec![ModuleView {
                module: "nomad".into(),
                steps: vec![
                    step(
                        "apt.package",
                        "nomad",
                        StepOutcome::Change,
                        Some(DiffKind::Update),
                    ),
                    step(
                        "file.template",
                        "/etc/nomad.d/nomad.hcl",
                        StepOutcome::Change,
                        Some(DiffKind::Update),
                    ),
                    step(
                        "file.absent",
                        "/etc/nomad.d/old.hcl",
                        StepOutcome::Change,
                        Some(DiffKind::Delete),
                    ),
                    step("systemd.unit", "nomad", StepOutcome::NoOp, None),
                    step("zfs.dataset", "tank", StepOutcome::Blocked, None),
                ],
                ..ModuleView::default()
            }],
            removals: vec![
                removal("grey", true, false),
                removal("grey-agent", true, true),
                removal("/opt/x", false, true),
            ],
            deferred: vec![ModuleNote {
                module: "otel".into(),
                reason: "waits for facts".into(),
                waiting_on: vec!["docker.version".into()],
                plan_error: false,
            }],
            ..PlanView::default()
        }
    }

    #[test]
    fn a_plan_tallies_its_steps_and_the_removals_that_run() {
        let t = plan_tally(&sample());
        assert_eq!((t.create, t.modify, t.remove, t.blocked), (0, 2, 3, 1));
    }

    #[test]
    fn a_hosts_tally_comes_from_its_plan_else_its_counts() {
        let mut h = host_entry(Some("5b1c9e02"));
        let group = PlanGroup {
            digest: "5b1c9e02".into(),
            hosts: vec!["pi-1".into()],
            changes: 3,
            removals: 2,
            destructive: true,
            plan: Some(sample()),
        };
        assert_eq!(
            host_tally(&h, std::slice::from_ref(&group)),
            Some(plan_tally(&sample()))
        );
        h.plan_changes = Some(4);
        h.plan_removals = Some(1);
        let t = host_tally(&h, &[]).unwrap();
        assert_eq!((t.modify, t.remove), (3, 1));
        assert_eq!(host_tally(&host_entry(None), &[group]), None);
    }

    fn host_entry(digest: Option<&str>) -> RolloutHost {
        RolloutHost {
            host: "pi-1".into(),
            phase: "canary".into(),
            wave: None,
            state: HostRolloutState::Planned,
            plan_digest: digest.map(Into::into),
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            reboot: None,
            needs_approval: false,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        }
    }

    #[test]
    fn a_plan_is_reviewed_by_what_the_server_flagged() {
        let r = review(&sample());
        assert_eq!((r.changes, r.unchanged, r.blocked), (3, 1, 1));
        assert_eq!(r.destructive_steps, 1);
        assert_eq!(
            (r.removals, r.forgotten, r.cautions),
            (2, 1, 1),
            "a caution on a removal that only forgets does not count"
        );
        assert_eq!(r.deferred, 1);
        assert_eq!(r.destructive(), 3);
    }

    #[test]
    fn an_approval_scope_names_the_hosts_digests_and_destructive_steps() {
        let group =
            |digest: &str, hosts: &[&str], plan: Option<PlanView>, removals: u32| PlanGroup {
                digest: digest.into(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
                changes: if plan.is_some() { 3 } else { 1 },
                removals,
                destructive: removals > 0,
                plan,
            };
        let plans = vec![
            group("5b1c9e02", &["oci-1", "oci-2"], Some(sample()), 2),
            group("e07aa341", &["oci-3"], None, 0),
            group("ffff0000", &["elsewhere"], None, 9),
        ];
        let host = |name: &str, digest: Option<&str>| RolloutHost {
            host: name.into(),
            phase: "cloud".into(),
            wave: None,
            state: HostRolloutState::Planned,
            plan_digest: digest.map(Into::into),
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            reboot: None,
            needs_approval: true,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        };
        let hosts = [
            host("oci-2", Some("5b1c9e02")),
            host("oci-1", None),
            host("oci-3", Some("e07aa341")),
            host("az-1", None),
        ];
        let refs: Vec<&RolloutHost> = hosts.iter().collect();
        let scope = approval_scope(&refs, &plans);
        assert_eq!(scope.groups, vec![0, 1]);
        assert_eq!(scope.digests, vec!["5b1c9e02", "e07aa341"]);
        assert_eq!(scope.hosts, vec!["oci-1", "oci-2", "oci-3"]);
        assert_eq!(scope.without_plan, vec!["az-1"]);
        // Counted once per distinct plan, not once per host; a group with
        // no plan document counts from its summary.
        assert_eq!(scope.destructive, 3);
        assert_eq!(scope.removals, 2);
        assert_eq!(scope.changes, 4);
    }
}
