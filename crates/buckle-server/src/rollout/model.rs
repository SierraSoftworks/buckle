//! The persisted shape of a rollout: what the controller checkpoints on every
//! transition so a restart resumes rather than restarts.

use buckle_api::{HostRolloutState, PhaseState, RolloutState};
use buckle_core::model::{ApprovalPolicy, Parallelism};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The name of the virtual final stage.
pub const STRAGGLERS: &str = "stragglers";

/// Per-host state inside a rollout. Finer than the UI's
/// [`HostRolloutState`], which [`HostPhase::api`] maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
    /// Assign + RequestPlan sent (or due when the host connects).
    PlanPending,
    /// A plan has arrived; waiting for approval and a wave slot.
    Planned,
    /// Baseline health requested.
    Baseline,
    /// Baseline failed; waiting for an operator to force or skip.
    Held,
    /// Apply sent; waiting for ApplyResult.
    Applying,
    /// Applied; checks repeating for the bake window.
    Baking,
    /// Applied and baked.
    Healthy,
    /// Apply, bake or a local rollback failed; counts against the budget.
    Failed,
    /// Excluded by an operator or by `baseline: skip`.
    Skipped,
    /// Never started during its phase (offline, plan pending); a straggler candidate.
    Missed,
    /// Rollback sent.
    RollingBack,
    /// Back on the previous stable revision.
    RolledBack,
}

impl HostPhase {
    /// The UI state.
    pub fn api(self) -> HostRolloutState {
        match self {
            HostPhase::PlanPending => HostRolloutState::PlanPending,
            HostPhase::Planned | HostPhase::Baseline => HostRolloutState::Planned,
            HostPhase::Held => HostRolloutState::Held,
            HostPhase::Applying | HostPhase::RollingBack => HostRolloutState::Applying,
            HostPhase::Baking => HostRolloutState::Baking,
            HostPhase::Healthy => HostRolloutState::Healthy,
            HostPhase::Failed => HostRolloutState::Failed,
            HostPhase::Skipped => HostRolloutState::Skipped,
            HostPhase::Missed => HostRolloutState::Pending,
            HostPhase::RolledBack => HostRolloutState::RolledBack,
        }
    }

    /// Occupies a wave slot.
    pub fn in_flight(self) -> bool {
        matches!(
            self,
            HostPhase::Baseline | HostPhase::Applying | HostPhase::Baking
        )
    }

    /// Nothing more will happen without an operator.
    pub fn finished(self) -> bool {
        matches!(
            self,
            HostPhase::Healthy
                | HostPhase::Failed
                | HostPhase::Skipped
                | HostPhase::Missed
                | HostPhase::RolledBack
        )
    }

    /// Stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            HostPhase::PlanPending => "plan_pending",
            HostPhase::Planned => "planned",
            HostPhase::Baseline => "baseline",
            HostPhase::Held => "held",
            HostPhase::Applying => "applying",
            HostPhase::Baking => "baking",
            HostPhase::Healthy => "healthy",
            HostPhase::Failed => "failed",
            HostPhase::Skipped => "skipped",
            HostPhase::Missed => "missed",
            HostPhase::RollingBack => "rolling_back",
            HostPhase::RolledBack => "rolled_back",
        }
    }

    /// Parses a stored name.
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
    }
}

/// Why a rollout is paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseInfo {
    /// `operator`, `failure_budget`, `gate`, `local_rollback`.
    pub kind: String,
    pub reason: String,
    #[serde(default)]
    pub host: Option<String>,
    pub at: DateTime<Utc>,
}

/// The pre-phase gate's progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRuntime {
    /// Hosts still to answer, with the request id each was sent.
    pub pending: BTreeMap<String, String>,
    pub passed: Vec<String>,
}

/// One phase as evaluated when the rollout started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRuntime {
    pub name: String,
    pub state: PhaseState,
    pub parallel: Parallelism,
    pub approval: ApprovalPolicy,
    pub bake_seconds: u64,
    pub max_failures: Parallelism,
    #[serde(default)]
    pub virtual_stage: bool,
    /// Failures an operator accepted with `resume`.
    #[serde(default)]
    pub accepted_failures: u32,
    /// Hosts started so far (for wave numbering).
    #[serde(default)]
    pub started: u32,
    #[serde(default)]
    pub gate: Option<GateRuntime>,
    #[serde(default)]
    pub bake_until: Option<DateTime<Utc>>,
}

impl PhaseRuntime {
    /// Failures the phase tolerates for `total` hosts: a count is taken as
    /// is, a percentage of the phase's hosts rounds down (so `10%` of nine
    /// hosts tolerates none).
    pub fn failure_budget(&self, total: usize) -> u32 {
        let base = match self.max_failures {
            Parallelism::Count(n) => n,
            Parallelism::Percent(p) => ((total as u64 * p as u64) / 100) as u32,
        };
        base + self.accepted_failures
    }
}

/// The rollout-level checkpoint beyond the indexed columns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutRuntime {
    pub phases: Vec<PhaseRuntime>,
    /// Index into `phases` of the phase being worked on.
    #[serde(default)]
    pub current_phase: usize,
    #[serde(default)]
    pub pause: Option<PauseInfo>,
    /// The stable revision when this rollout started (what rollback applies).
    #[serde(default)]
    pub previous_stable: Option<String>,
    /// The virtual stragglers stage is accepting hosts.
    #[serde(default)]
    pub stragglers_open: bool,
    #[serde(default)]
    pub trace_id: Option<String>,
    /// W3C traceparent of the `rollout.run` span.
    #[serde(default)]
    pub traceparent: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub revision_message: String,
    /// While rolling back: the phase index being rolled back.
    #[serde(default)]
    pub rollback_phase: Option<usize>,
    /// Why the rollout ended, when it did not complete.
    #[serde(default)]
    pub end_reason: Option<String>,
    /// Hosts the target-set freeze matched to no phase.
    #[serde(default)]
    pub untargeted: Vec<String>,
}

/// One rollout row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutRecord {
    pub id: String,
    pub revision: String,
    pub state: RolloutState,
    /// The rollout policy as it was in the revision.
    pub policy: buckle_core::model::RolloutPolicy,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub triggered_by: String,
    pub runtime: RolloutRuntime,
}

impl RolloutRecord {
    /// Planning, awaiting approval, rolling or baking.
    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            RolloutState::Planning
                | RolloutState::AwaitingApproval
                | RolloutState::Rolling
                | RolloutState::Baking
        )
    }

    /// Started and not yet in a terminal state (paused and rolling back count).
    pub fn is_active(&self) -> bool {
        self.is_running() || matches!(self.state, RolloutState::Paused | RolloutState::RollingBack)
    }

    /// Ended (completed, rolled back, aborted).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            RolloutState::Completed | RolloutState::RolledBack | RolloutState::Aborted
        )
    }
}

/// The per-host checkpoint beyond the indexed columns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRuntime {
    /// The revision this host is being moved to (the rollout's, or the
    /// stable one in the stragglers stage).
    pub revision: String,
    #[serde(default)]
    pub plan_changes: Option<u32>,
    #[serde(default)]
    pub plan_removals: Option<u32>,
    #[serde(default)]
    pub destructive: bool,
    /// Outstanding RunHealth request (baseline).
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub bake_until: Option<DateTime<Utc>>,
    /// Required checks currently failing during bake, since when.
    #[serde(default)]
    pub failing_since: BTreeMap<String, DateTime<Utc>>,
    /// Checks that failed and then recovered inside the settle window.
    #[serde(default)]
    pub flaps: Vec<String>,
    #[serde(default)]
    pub failed_module: Option<String>,
    #[serde(default)]
    pub failed_checks: Vec<String>,
    #[serde(default)]
    pub waived_checks: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Apply was sent (the host may be on the new revision).
    #[serde(default)]
    pub applied: bool,
    /// An operator forced the apply past a failing baseline.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub disconnected_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub apply_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub attempts: u32,
    /// The phase index the host belongs to (stragglers use `usize::MAX` → None).
    #[serde(default)]
    pub phase_index: Option<usize>,
    /// A rollback the host ran on its own failed; both results are recorded.
    #[serde(default)]
    pub needs_attention: bool,
    /// How often the Apply was re-sent after reconnects without being run.
    #[serde(default)]
    pub apply_resends: u32,
    /// An operator retried or continued this host: it may proceed while the
    /// rollout is paused by a failure (never under an operator's pause).
    /// Cleared when the host finishes.
    #[serde(default)]
    pub released: bool,
    /// Bake health reports received since the bake (re)started; a host with
    /// a bake window completes only once it has reported.
    #[serde(default)]
    pub bake_reports: u32,
    /// The bake requests sent during this bake (reports answering others,
    /// e.g. another rollout's, are not this bake's).
    #[serde(default)]
    pub bake_requests: Vec<String>,
    /// Modules the plan defers to apply time (their plan is unknown yet).
    #[serde(default)]
    pub deferred_modules: Vec<String>,
    /// The host was held (or skipped by `baseline: skip`) at baseline; if it
    /// was never applied it moves to the stragglers stage on promotion.
    #[serde(default)]
    pub held_at_baseline: bool,
}

/// One `rollout_hosts` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutHostRecord {
    pub rollout: String,
    pub host: String,
    pub phase: String,
    pub wave: Option<u32>,
    pub state: HostPhase,
    pub plan_digest: Option<String>,
    pub plan: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub health: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub runtime: HostRuntime,
}
