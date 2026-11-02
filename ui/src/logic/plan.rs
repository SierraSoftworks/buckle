//! Reading a plan for review (design §11).
//!
//! `PlanGroup.plan` is the server's typed [`PlanView`] of the engine's
//! redacted plan, so what is destructive, a removal or a caution is the
//! server's call; this module only counts what an approver is asked to
//! accept.

use std::collections::BTreeSet;

use buckle_api::{PlanGroup, PlanView, RolloutHost, StepOutcome};

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
            needs_approval: true,
            needs_attention: false,
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
