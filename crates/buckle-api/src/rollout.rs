use crate::{HostSummary, PlanView, RevisionSummary};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    Detected,
    Planning,
    AwaitingApproval,
    Rolling,
    Baking,
    Paused,
    RollingBack,
    RolledBack,
    Completed,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseState {
    Pending,
    AwaitingApproval,
    Running,
    Baking,
    Done,
    Paused,
    Skipped,
    /// For the stragglers stage: accepting hosts as they appear.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRolloutState {
    Pending,
    PlanPending,
    Planned,
    Held,
    Applying,
    Baking,
    Healthy,
    Failed,
    RolledBack,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutHost {
    pub host: String,
    pub phase: String,
    pub wave: Option<u32>,
    pub state: HostRolloutState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_changes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_removals: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_module: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived_checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake_until: Option<DateTime<Utc>>,
    /// The host's current plan is not covered by an approval of its phase
    /// (never approved, or the plan changed after approval).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutPhase {
    pub name: String,
    pub state: PhaseState,
    pub parallel: String,
    pub approval: String,
    pub bake_seconds: u64,
    pub max_failures: String,
    pub virtual_stage: bool,
    pub hosts: Vec<RolloutHost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake_until: Option<DateTime<Utc>>,
    /// The health gate: the required checks (`module/check`) of the modules
    /// selected for this phase's hosts, which baseline, bake and the
    /// pre-phase re-check hold the phase to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutSummary {
    pub id: String,
    pub revision: String,
    pub revision_message: String,
    pub state: RolloutState,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub triggered_by: String,
    pub hosts_total: u32,
    pub hosts_done: u32,
    pub hosts_failed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// A link to `trace_id` in the configured tracing UI (Honeycomb), when the
    /// server knows one (otherwise the UI shows the id as text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutDetail {
    #[serde(flatten)]
    pub summary: RolloutSummary,
    pub phases: Vec<RolloutPhase>,
    /// Distinct plans across hosts, for review.
    pub plans: Vec<PlanGroup>,
    pub events: Vec<RolloutEvent>,
}

/// Hosts sharing one plan digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGroup {
    pub digest: String,
    pub hosts: Vec<String>,
    pub changes: u32,
    pub removals: u32,
    pub destructive: bool,
    /// The plan, as one of `hosts` planned it. `None` when the server holds
    /// no readable copy (the counts above still come from the host's report).
    #[serde(default)]
    pub plan: Option<PlanView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutEvent {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// `POST /api/v1/rollouts/{id}/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveRequest {
    pub phase: String,
    /// The plan digests being approved; the server rejects if a host's plan differs.
    pub plan_digests: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// `POST /api/v1/rollouts/{id}/reject`: turns a rollout down while it waits
/// for approval. It ends aborted, with the reason in the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectRequest {
    /// Why; required, and recorded with the `rollout.reject` audit entry.
    pub reason: String,
}

/// `POST /api/v1/rollouts/{id}/hosts/{host}/continue`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinueRequest {
    /// `module/check` names to waive for this host in this rollout.
    pub waive_checks: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequest {
    #[serde(default)]
    pub note: String,
}

/// Server-sent event payloads on `GET /api/v1/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    HostChanged {
        host: HostSummary,
    },
    RolloutChanged {
        rollout: RolloutSummary,
    },
    RolloutHostChanged {
        rollout_id: String,
        host: RolloutHost,
    },
    RevisionAdded {
        revision: RevisionSummary,
    },
    Audit {
        entry: AuditEntryRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntryRef {
    pub id: u64,
    pub at: DateTime<Utc>,
    pub action: String,
    pub message: String,
}
