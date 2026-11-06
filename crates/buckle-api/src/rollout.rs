use crate::{FleetPlan, HostSummary, PlanView, RevisionSummary};
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
    /// Never started: a newer revision superseded it while it was queued.
    Skipped,
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
    /// Applied; the run asked for a reboot and the rollout waits for the
    /// agent to come back from a new boot before it bakes.
    Rebooting,
    Baking,
    Healthy,
    Failed,
    RolledBack,
    Skipped,
}

/// How long a target's step bakes: its phase's `bake`, raised by the `bake`
/// of each module (or workload) its run changed, or not at all when the run
/// changed nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BakeWindow {
    pub seconds: u64,
    /// The changed module (or workload) whose own `bake` set the window,
    /// when it is longer than the phase's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The run changed nothing, so the step does not bake.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutHost {
    /// The host, or for a cluster's rows the cluster.
    pub host: String,
    /// For a workload placement: the workload rolled out on the cluster
    /// `host` names. Each placement is its own row (`kind: workload`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
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
    /// A rollback the host ran on its own (local or module-scoped) failed:
    /// the original failure and the rollback's own result are both in
    /// `error` (and the stored result), and an operator must look.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_attention: bool,
    /// `error` as a structured error (message, causes, advice), when the agent
    /// reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<crate::run::ErrorView>,
    /// The host's run for this rollout, for `GET /rollouts/{id}/hosts/{host}/report`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The reboot the host's run asked for in this rollout, once it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot: Option<HostReboot>,
    /// A host, a whole cluster applied through an executor (an executor
    /// that predates placements), or one workload placement on a cluster.
    #[serde(default)]
    pub kind: crate::TargetKind,
    /// The dependency step inside the phase the target's current step runs
    /// in (0-based): `needs` edges between the phase's targets (a workload
    /// after the module or workload it needs, a host's dependent modules
    /// after the workload) put the dependent in a later step.
    #[serde(default, alias = "stage")]
    pub step: u32,
    /// For a cluster: the host agent speaking for it in this rollout, once
    /// one is picked (`None` reads "awaiting executor").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// The bake window the current step resolved to from what its run
    /// changed, once its result arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake: Option<BakeWindow>,
    /// The plan was made before the target's stage opened (at the rollout's
    /// start): an estimate, planned again when the stage opens.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
    /// The digest of the preview the current plan differs from: what will
    /// apply changed between the rollout's start and the stage opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drifted_from: Option<String>,
    /// A placement whose workload left the configuration: it removes what
    /// the workload owned on the cluster.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removal: bool,
    /// Plans the target applied earlier in this rollout, oldest first: an
    /// attempt before a retry (or force apply), or an earlier step. The
    /// current plan (`plan_digest`) is not among them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<RolloutAttempt>,
    /// What the rollout changes on the target across its attempts and
    /// steps: the current plan with what the earlier ones applied folded in
    /// (see [`crate::PlanView::absorb`]). Only set with `earlier`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<crate::ChangeCounts>,
}

/// A plan a rollout target applied before it was planned again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutAttempt {
    /// The target's attempt the plan belonged to, from 1.
    pub attempt: u32,
    /// The dependency step it applied in (see [`RolloutHost::step`]).
    #[serde(default)]
    pub step: u32,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removals: Option<u32>,
    /// Where the attempt ended (failed, healthy, …).
    pub state: HostRolloutState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When it was planned again.
    pub at: DateTime<Utc>,
    /// The attempt's run, for `GET /hosts/{host}/runs/{run}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// A reboot a rollout host's run asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReboot {
    /// What asked for it: `module/kind[key]` or `owner:handler`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_by: Vec<String>,
    /// When the result announcing it arrived.
    pub since: DateTime<Utc>,
    /// When the host fails if it has not come back from a new boot.
    pub until: DateTime<Utc>,
    /// When the agent came back from the reboot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back_at: Option<DateTime<Utc>>,
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
    /// How many dependency steps the phase's targets are laid out in (1
    /// when no `needs` edge crosses between its targets).
    #[serde(default = "one")]
    pub stages: u32,
    /// How many of `hosts` finished without changing anything (their plan
    /// had no changes and no removals): they are counted, not rolled.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutSummary {
    pub id: String,
    /// The memorable `word.word.word` form of `id` ([`crate::rollout_name`]),
    /// accepted wherever the API takes a rollout id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub revision: String,
    /// The revision's full commit message (subject, then body); rollouts
    /// started before the body was kept carry the subject alone.
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
    /// Why a finished rollout ended (`superseded by …`, `aborted by …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// The newer rollout that skipped (or replaced) this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// A link to `trace_id` in the configured tracing UI (Honeycomb), when the
    /// server knows one (otherwise the UI shows the id as text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
    /// An action rollout: the action each target runs in place of an apply
    /// of `revision` (whose phases it follows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RolloutActionInfo>,
    /// What the rollout changes across its fleet ([`crate::FleetPlan::changes`]),
    /// once any host has planned. Lists and the detail carry it; live
    /// updates leave it out, so a client keeps the one it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<crate::ChangeCounts>,
}

/// The action an action rollout runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutActionInfo {
    /// The module (a cluster's: the workload) declaring it.
    pub module: String,
    pub action: String,
    pub label: String,
    /// The selector the targets were picked by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Where each target runs it, when the operator picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<crate::ActionInstances>,
}

/// `POST /api/v1/rollouts`: run an action on every target offering it (that
/// `selector` matches) through the stable revision's phases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartActionRollout {
    pub module: String,
    pub action: String,
    /// The action is the workload `module`'s, run on the clusters running
    /// it (one placement each); else a host module's.
    #[serde(default)]
    pub workload: bool,
    /// A filter over the targets' context (`labels.role == "web"`); absent
    /// for every target offering the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Required when the action asks for confirmation.
    #[serde(default)]
    pub note: String,
    /// Where each target runs it, for an action offering the choice
    /// ([`crate::ActionInfo::instances`]); absent: its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<crate::ActionInstances>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutDetail {
    #[serde(flatten)]
    pub summary: RolloutSummary,
    pub phases: Vec<RolloutPhase>,
    /// Distinct plans across hosts, for review.
    pub plans: Vec<PlanGroup>,
    /// Every host's plan of the rollout's revision as one union, each change
    /// marked with the hosts it applies to (see [`FleetPlan`]). `None` from
    /// servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetPlan>,
    /// The timeline, oldest first (the newest 200 entries); later entries
    /// arrive as [`LiveEvent::RolloutTimeline`].
    pub events: Vec<RolloutEvent>,
}

/// Hosts sharing one plan digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGroup {
    pub digest: String,
    /// Hosts by id; a workload placement as `cluster/workload`
    /// ([`crate::placement_key`]).
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
    /// The host, or the cluster of the placement, the event is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The workload, when the event is about one placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
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

/// `POST /api/v1/targets/{id}/actions/{module}/{action}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunActionRequest {
    /// Required when the action asks for confirmation.
    #[serde(default)]
    pub note: String,
    /// Where an action offering the choice ([`crate::ActionInfo::instances`])
    /// runs; absent: its default. Ignored by other actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<crate::ActionInstances>,
}

/// `POST /api/v1/targets/{id}/steward`: move a cluster's lease to `host`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StewardRequest {
    pub host: String,
    #[serde(default)]
    pub note: String,
}

/// `POST /api/v1/{hosts|targets}/{id}/drift`: check the target for drift
/// now; for a cluster, only `workload`'s placement when named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftCheckRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default)]
    pub note: String,
}

/// Server-sent event payloads on `GET /api/v1/events`.
// Built once and serialised at once: a host summary's size is no concern.
#[allow(clippy::large_enum_variant)]
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
    /// An entry was added to a rollout's timeline (`RolloutDetail.events`).
    RolloutTimeline {
        rollout_id: String,
        event: RolloutEvent,
    },
    RevisionAdded {
        revision: RevisionSummary,
    },
    /// A step or module of a host's run changed state (SSE event `run_progress`).
    RunProgress {
        progress: crate::run::RunProgress,
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
