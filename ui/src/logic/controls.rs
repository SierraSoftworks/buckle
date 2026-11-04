//! The rollout-wide decisions an operator is offered, as one split button:
//! the decision the rollout is waiting on (approve, else pause or resume) on
//! its face, and the rest behind its menu.

use buckle_api::{RevisionStatus, RevisionSummary, RolloutState, RolloutSummary};
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

/// What an operator can decide for a rollout in `state`, with `approve` the
/// phase an approval is wanted for (and has plans to bind). Nothing once the
/// rollout has finished.
pub fn decisions(state: RolloutState, approve: Option<&str>) -> Option<Decisions> {
    if rollout_finished(state) {
        return None;
    }
    let hold = match state {
        RolloutState::Paused => Some(Decision::Resume),
        RolloutState::RollingBack => None,
        _ => Some(Decision::Pause),
    };
    let mut all: Vec<Decision> = approve
        .map(|p| Decision::Approve(p.to_string()))
        .into_iter()
        .chain(hold)
        .collect();
    if state == RolloutState::AwaitingApproval {
        all.push(Decision::Reject);
    }
    if matches!(
        state,
        RolloutState::Paused | RolloutState::Rolling | RolloutState::Baking
    ) {
        all.push(Decision::Rollback);
    }
    all.push(Decision::Abort);
    let primary = all.remove(0);
    Some(Decisions { primary, menu: all })
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
            let d = decisions(state, approve).expect("a live rollout has decisions");
            assert_eq!(d.primary, face, "{state:?}");
            assert!(
                !d.menu.contains(&face),
                "{state:?}: the face is not repeated"
            );
        }
    }

    #[test]
    fn the_menu_holds_abort_and_rollback_where_they_apply() {
        let menu = |s, a| decisions(s, a).unwrap().menu;
        assert!(menu(RolloutState::Rolling, None).contains(&Rollback));
        assert!(menu(RolloutState::Rolling, None).contains(&Abort));
        assert!(menu(RolloutState::Paused, None).contains(&Rollback));
        assert!(!menu(RolloutState::Planning, None).contains(&Rollback));
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
            assert_eq!(decisions(s, None), None);
        }
    }
}
