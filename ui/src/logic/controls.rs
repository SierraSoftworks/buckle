//! The rollout-wide decisions an operator is offered, as one split button:
//! the decision the rollout is waiting on (approve, else pause, retry or
//! resume) on its face, and the rest behind its menu.

use buckle_api::{HostRolloutState, RevisionStatus, RevisionSummary, RolloutState, RolloutSummary};
use chrono::{DateTime, Utc};

use super::tone::{rollout_active, rollout_finished};

/// Where a revision stands, as the one control (or marker) its card shows
/// to the right of the commit hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stance {
    /// Rejected by validation: it cannot be deployed.
    Corrupt,
    /// The release the fleet is on.
    Stable,
    /// Older than the stable release: roll the fleet back to it.
    Rollback,
    /// A proposed rollout waiting on a decision.
    Review,
    /// A rollout in progress that needs nothing from the operator.
    Running,
    /// Newer than the stable release and not rolled out (a manual trigger).
    Start,
}

/// The stance of `rev` given its `rollout` and the creation time of the
/// stable revision (`stable_at`).
pub fn stance(
    rev: &RevisionSummary,
    rollout: Option<&RolloutSummary>,
    stable_at: Option<DateTime<Utc>>,
) -> Option<Stance> {
    if rev.status == RevisionStatus::Rejected {
        return Some(Stance::Corrupt);
    }
    if rev.is_stable {
        return Some(Stance::Stable);
    }
    if let Some(r) = rollout.filter(|r| rollout_active(r.state)) {
        return Some(if r.state == RolloutState::AwaitingApproval {
            Stance::Review
        } else {
            Stance::Running
        });
    }
    if stable_at.is_some_and(|s| rev.created_at < s) {
        return Some(Stance::Rollback);
    }
    // A rollout queued behind the running one speaks for itself.
    rollout
        .is_none_or(|r| rollout_finished(r.state))
        .then_some(Stance::Start)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Approve this phase, once it has plans to bind.
    Approve(String),
    Reject,
    Pause,
    Resume,
    /// Re-plan the rollout's failed hosts.
    Retry,
    /// Resume past failures: accept them and move on (the resume of a
    /// rollout that paused with failed hosts).
    SkipPhase,
    /// Count the baking hosts as baked now.
    Expedite,
    Rollback,
    Abort,
}

impl Decision {
    pub fn label(&self) -> String {
        match self {
            Decision::Approve(phase) => format!("Approve {phase}…"),
            Decision::Reject => "Reject…".into(),
            Decision::Pause => "Pause".into(),
            Decision::Resume => "Resume".into(),
            Decision::Retry => "Retry".into(),
            Decision::SkipPhase => "Skip this phase…".into(),
            Decision::Expedite => "Expedite…".into(),
            Decision::Rollback => "Roll back…".into(),
            Decision::Abort => "Abort…".into(),
        }
    }

    /// Ends or undoes the rollout: confirmed by typing its name.
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Decision::Reject | Decision::Rollback | Decision::Abort
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decisions {
    pub primary: Decision,
    /// The rest, the gentle ones first.
    pub menu: Vec<Decision>,
}

/// What the rollout's hosts call for: how many failed, how many bake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Standing {
    pub failed: usize,
    pub baking: usize,
    /// An action rollout: there is nothing to roll back to.
    pub action: bool,
}

/// What an operator can decide for a rollout in `state`, with `approve` the
/// phase an approval is wanted for (and has plans to bind). A rollout that
/// paused with failed hosts offers retrying them on its face, and resuming
/// past them as skipping the phase. Nothing once the rollout has finished.
pub fn decisions(state: RolloutState, approve: Option<&str>, hosts: Standing) -> Option<Decisions> {
    if rollout_finished(state) {
        return None;
    }
    let failed = hosts.failed > 0;
    let hold: &[Decision] = match state {
        RolloutState::Paused if failed => &[Decision::Retry, Decision::SkipPhase],
        RolloutState::Paused => &[Decision::Resume],
        RolloutState::RollingBack => &[],
        _ if failed => &[Decision::Pause, Decision::Retry],
        _ => &[Decision::Pause],
    };
    let mut all: Vec<Decision> = approve
        .map(|p| Decision::Approve(p.to_string()))
        .into_iter()
        .chain(hold.iter().cloned())
        .collect();
    if hosts.baking > 0 && state != RolloutState::RollingBack {
        all.push(Decision::Expedite);
    }
    if state == RolloutState::AwaitingApproval {
        all.push(Decision::Reject);
    }
    if !hosts.action
        && matches!(
            state,
            RolloutState::Paused | RolloutState::Rolling | RolloutState::Baking
        )
    {
        all.push(Decision::Rollback);
    }
    all.push(Decision::Abort);
    let primary = all.remove(0);
    Some(Decisions { primary, menu: all })
}

/// What re-planning a host amounts to, as its button says it: a host that
/// failed past its plan (its apply or bake failed, it was held, skipped or
/// missed with a plan in hand) is planned and applied again, a "Retry"; one
/// whose planning failed, or that is still waiting to apply, only plans again.
pub fn replan_label(state: HostRolloutState, has_plan: bool) -> &'static str {
    let waiting = matches!(
        state,
        HostRolloutState::PlanPending | HostRolloutState::Planned
    );
    if has_plan && !waiting {
        "Retry"
    } else {
        "Re-plan"
    }
}

/// A host's (or cluster's) own actions, as its page head offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetAction {
    ApproveRegistration,
    /// Review and approve the rollout phase waiting on it.
    ApproveRollout,
    UpgradeAgent,
    Pause,
    Resume,
    Forget,
}

impl TargetAction {
    pub fn label(self) -> &'static str {
        match self {
            TargetAction::ApproveRegistration => "Approve registration…",
            TargetAction::ApproveRollout => "Approve rollout…",
            TargetAction::UpgradeAgent => "Upgrade agent…",
            TargetAction::Pause => "Pause…",
            TargetAction::Resume => "Resume",
            TargetAction::Forget => "Forget…",
        }
    }
}

/// What a target's head can offer: its state, and what calls for action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetStanding {
    pub pending: bool,
    pub paused: bool,
    /// A rollout wants an approval for a phase that holds it.
    pub approval_wanted: bool,
    /// Its agent can be upgraded (a host, online).
    pub upgradable: bool,
    /// Its agent is behind the version the fleet converges to.
    pub behind: bool,
    pub forgettable: bool,
}

/// A target's own actions, the most likely first (the split button's face)
/// and Forget last: approve its registration, then a rollout waiting on it,
/// then upgrade an agent that is behind, then pause or resume. The second
/// is whether the face is what the target's state calls for (a primary
/// button) rather than the routine choice.
pub fn head_actions(s: TargetStanding) -> (Vec<TargetAction>, bool) {
    use TargetAction::*;
    let upgrade_first = s.upgradable && s.behind;
    let all: Vec<TargetAction> = [
        (s.pending, ApproveRegistration),
        (s.approval_wanted, ApproveRollout),
        (upgrade_first, UpgradeAgent),
        (s.paused, Resume),
        (!s.paused, Pause),
        (s.upgradable && !upgrade_first, UpgradeAgent),
        (s.forgettable, Forget),
    ]
    .into_iter()
    .filter_map(|(on, a)| on.then_some(a))
    .collect();
    let called_for = matches!(
        all.first(),
        Some(ApproveRegistration | ApproveRollout | UpgradeAgent | Resume)
    );
    (all, called_for)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Decision::*;

    fn revision(stable: bool, age_days: i64, status: RevisionStatus) -> RevisionSummary {
        RevisionSummary {
            id: "r".into(),
            commit: "abc".into(),
            author: "a".into(),
            message: "m".into(),
            created_at: Utc::now() - chrono::Duration::days(age_days),
            status,
            validation_errors: vec![],
            is_stable: stable,
            rollout_id: None,
            module_count: 1,
            workload_count: 0,
        }
    }

    fn rollout(state: RolloutState) -> RolloutSummary {
        serde_json::from_value(serde_json::json!({
            "id": "1", "revision": "r", "revision_message": "m", "state": state,
            "started_at": Utc::now(), "triggered_by": "t",
            "hosts_total": 1, "hosts_done": 0, "hosts_failed": 0,
        }))
        .unwrap()
    }

    #[test]
    fn a_revision_has_the_one_stance_that_fits() {
        use RevisionStatus::{Rejected, Valid};
        use RolloutState::*;
        let stable_at = Some(Utc::now() - chrono::Duration::days(2));
        let cases = [
            (revision(true, 2, Valid), None, Some(Stance::Stable)),
            (revision(false, 5, Valid), None, Some(Stance::Rollback)),
            (
                revision(false, 5, Valid),
                Some(Aborted),
                Some(Stance::Rollback),
            ),
            (
                revision(false, 1, Valid),
                Some(AwaitingApproval),
                Some(Stance::Review),
            ),
            (
                revision(false, 1, Valid),
                Some(Rolling),
                Some(Stance::Running),
            ),
            (
                revision(false, 1, Valid),
                Some(Baking),
                Some(Stance::Running),
            ),
            // Rolling back to an older revision is a rollout in progress.
            (
                revision(false, 5, Valid),
                Some(Rolling),
                Some(Stance::Running),
            ),
            (revision(false, 1, Valid), None, Some(Stance::Start)),
            (
                revision(false, 1, Valid),
                Some(Aborted),
                Some(Stance::Start),
            ),
            (revision(false, 1, Valid), Some(Detected), None),
            (revision(false, 1, Rejected), None, Some(Stance::Corrupt)),
            (revision(false, 5, Rejected), None, Some(Stance::Corrupt)),
        ];
        for (rev, state, want) in cases {
            let r = state.map(rollout);
            assert_eq!(stance(&rev, r.as_ref(), stable_at), want, "{state:?}");
        }
    }

    #[test]
    fn the_face_is_what_the_rollout_waits_on() {
        let cases = [
            (
                RolloutState::AwaitingApproval,
                Some("canary"),
                Approve("canary".into()),
            ),
            (RolloutState::Rolling, Some("web"), Approve("web".into())),
            // No plans to bind yet: nothing to approve.
            (RolloutState::AwaitingApproval, None, Pause),
            (RolloutState::Rolling, None, Pause),
            (RolloutState::Baking, None, Pause),
            (RolloutState::Paused, None, Resume),
            (RolloutState::RollingBack, None, Abort),
        ];
        for (state, approve, face) in cases {
            let d = decisions(state, approve, Standing::default())
                .expect("a live rollout has decisions");
            assert_eq!(d.primary, face, "{state:?}");
            assert!(
                !d.menu.contains(&face),
                "{state:?}: the face is not repeated"
            );
        }
    }

    #[test]
    fn the_menu_holds_abort_and_rollback_where_they_apply() {
        let menu = |s, a| decisions(s, a, Standing::default()).unwrap().menu;
        assert!(menu(RolloutState::Rolling, None).contains(&Rollback));
        assert!(menu(RolloutState::Rolling, None).contains(&Abort));
        assert!(menu(RolloutState::Paused, None).contains(&Rollback));
        assert!(!menu(RolloutState::Planning, None).contains(&Rollback));
        let action = Standing {
            action: true,
            ..Default::default()
        };
        let paused = decisions(RolloutState::Paused, None, action).unwrap().menu;
        assert!(
            !paused.contains(&Rollback),
            "an action rollout has nothing to roll back to"
        );
        assert!(paused.contains(&Abort));
        assert!(menu(RolloutState::AwaitingApproval, Some("canary")).contains(&Reject));
        // Approving does not take pausing away.
        assert!(menu(RolloutState::Rolling, Some("web")).contains(&Pause));
        assert!(menu(RolloutState::Paused, Some("web")).contains(&Resume));
    }

    #[test]
    fn a_finished_rollout_has_none() {
        for s in [
            RolloutState::Completed,
            RolloutState::RolledBack,
            RolloutState::Aborted,
        ] {
            assert_eq!(decisions(s, None, Standing::default()), None);
        }
    }

    #[test]
    fn failed_hosts_put_retry_first_and_resume_becomes_skipping_the_phase() {
        let failed = Standing {
            failed: 2,
            ..Default::default()
        };
        let d = decisions(RolloutState::Paused, None, failed).unwrap();
        assert_eq!(d.primary, Retry);
        assert!(d.menu.contains(&SkipPhase));
        assert!(!d.menu.contains(&Resume));
        // Running with failures within budget: retry is in the menu.
        let d = decisions(RolloutState::Rolling, None, failed).unwrap();
        assert_eq!(d.primary, Pause);
        assert!(d.menu.contains(&Retry));
        assert!(!d.menu.contains(&SkipPhase));
    }

    #[test]
    fn baking_hosts_offer_skipping_the_bake() {
        let baking = Standing {
            baking: 1,
            ..Default::default()
        };
        for (state, offered) in [
            (RolloutState::Baking, true),
            (RolloutState::Paused, true),
            (RolloutState::RollingBack, false),
        ] {
            let d = decisions(state, None, baking).unwrap();
            assert_eq!(d.menu.contains(&Expedite), offered, "{state:?}");
        }
        let d = decisions(RolloutState::Baking, None, Standing::default()).unwrap();
        assert!(!d.menu.contains(&Expedite));
    }

    #[test]
    fn a_host_that_failed_past_its_plan_is_retried_else_replanned() {
        use HostRolloutState as S;
        for (state, has_plan, want) in [
            (S::Failed, true, "Retry"),
            (S::Held, true, "Retry"),
            (S::Pending, true, "Retry"),
            (S::Failed, false, "Re-plan"),
            (S::Planned, true, "Re-plan"),
            (S::PlanPending, false, "Re-plan"),
        ] {
            assert_eq!(replan_label(state, has_plan), want, "{state:?} {has_plan}");
        }
    }

    #[test]
    fn a_targets_head_puts_what_its_state_calls_for_first() {
        use TargetAction::*;
        let base = TargetStanding {
            upgradable: true,
            forgettable: true,
            ..Default::default()
        };
        for (case, s, face, called_for) in [
            ("routine", base, Pause, false),
            (
                "paused",
                TargetStanding {
                    paused: true,
                    ..base
                },
                Resume,
                true,
            ),
            (
                "behind",
                TargetStanding {
                    behind: true,
                    ..base
                },
                UpgradeAgent,
                true,
            ),
            (
                "rollout waiting",
                TargetStanding {
                    approval_wanted: true,
                    behind: true,
                    ..base
                },
                ApproveRollout,
                true,
            ),
            (
                "pending",
                TargetStanding {
                    pending: true,
                    approval_wanted: true,
                    ..base
                },
                ApproveRegistration,
                true,
            ),
        ] {
            let (all, primary) = head_actions(s);
            assert_eq!(all.first(), Some(&face), "{case}");
            assert_eq!(primary, called_for, "{case}");
            assert_eq!(all.last(), Some(&Forget), "{case}: forget comes last");
            assert_eq!(
                all.iter().filter(|a| **a == UpgradeAgent).count(),
                1,
                "{case}"
            );
        }
    }

    #[test]
    fn a_target_offers_only_what_it_can_do() {
        let (all, _) = head_actions(TargetStanding::default());
        assert_eq!(all, vec![TargetAction::Pause]);
    }
}
