//! The rollout-wide decisions an operator is offered, as one split button:
//! the decision the rollout is waiting on (approve, else pause or resume) on
//! its face, and the rest behind its menu.

use buckle_api::RolloutState;

use super::tone::rollout_finished;

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
