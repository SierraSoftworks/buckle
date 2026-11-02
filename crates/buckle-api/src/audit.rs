use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: u64,
    pub at: DateTime<Utc>,
    /// `operator` subject or `controller`.
    pub actor: String,
    pub action: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Controller transitions that only show a host moving through a rollout's
/// ordinary steps; outcomes such as healthy, failed, held, skipped or
/// rolled back are not progress. `GET /api/v1/audit?progress=false` leaves
/// them out, and filters can name them as `progress`.
pub const PROGRESS_ACTIONS: &[&str] = &[
    "rollout.host.plan_pending",
    "rollout.host.planned",
    "rollout.host.baseline",
    "rollout.host.applying",
    "rollout.host.baking",
    "rollout.host.rolling_back",
];

/// Whether an entry by `actor` with `action` is a progress transition.
pub fn is_progress_action(actor: &str, action: &str) -> bool {
    actor == "controller" && PROGRESS_ACTIONS.contains(&action)
}

impl AuditEntry {
    /// A controller progress transition (see [`PROGRESS_ACTIONS`]).
    pub fn is_progress(&self) -> bool {
        is_progress_action(&self.actor, &self.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("controller", "rollout.host.applying", true)]
    #[case("controller", "rollout.host.healthy", false)]
    #[case("controller", "rollout.host.failed", false)]
    #[case("alice", "rollout.host.applying", false)]
    fn only_the_controllers_ordinary_steps_are_progress(
        #[case] actor: &str,
        #[case] action: &str,
        #[case] progress: bool,
    ) {
        assert_eq!(is_progress_action(actor, action), progress);
    }
}
