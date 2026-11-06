//! The persisted shape of a rollout: what the controller checkpoints on every
//! transition so a restart resumes rather than restarts.

use super::stages::Step;
use buckle_api::{HostRolloutState, PhaseState, RolloutState, TargetKind};
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
    /// Applied, and the run asked for a reboot: waiting for the agent to
    /// come back from a new boot (or the reboot timeout) before baking.
    Rebooting,
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
            HostPhase::Rebooting => HostRolloutState::Rebooting,
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
            HostPhase::Baseline | HostPhase::Applying | HostPhase::Rebooting | HostPhase::Baking
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
            HostPhase::Rebooting => "rebooting",
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
    /// How many dependency stages the phase's targets are laid out in.
    #[serde(default = "one")]
    pub stages: u32,
    /// The highest stage opened so far (its targets may plan and start).
    #[serde(default)]
    pub open_stage: u32,
}

fn one() -> u32 {
    1
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
    /// The newer rollout that skipped (or replaced) this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Hosts the target-set freeze matched to no phase.
    #[serde(default)]
    pub untargeted: Vec<String>,
    /// An action rollout: what each target runs in place of an apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RolloutAction>,
}

/// The action an action rollout runs on each of its targets, from the
/// revision each runs (in place of a plan and an apply of the rollout's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutAction {
    /// The module (a cluster's: the workload) declaring it.
    pub module: String,
    pub action: String,
    pub label: String,
    /// It asks for confirmation: an `if_destructive` phase needs approval.
    #[serde(default)]
    pub confirm: bool,
    /// The selector the targets were picked by, as the operator wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

impl RolloutAction {
    /// `module/action`, as runs name it.
    pub fn key(&self) -> String {
        format!("{}/{}", self.module, self.action)
    }

    /// What every target's "plan" is: one digest, so one approval covers
    /// the phase.
    pub fn digest(&self) -> String {
        buckle_core::Digest::of(format!("action:{}", self.key()).as_bytes())
            .as_str()
            .to_owned()
    }
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

    /// Ended (completed, rolled back, aborted, skipped).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            RolloutState::Completed
                | RolloutState::RolledBack
                | RolloutState::Aborted
                | RolloutState::Skipped
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
    /// The bake window the current step resolved to from what its run
    /// changed; absent until then (the phase's `bake` applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake: Option<super::bake::BakeWindow>,
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
    /// The last failed apply's error, structured (from the agent's
    /// ApplyResult), stored once when the result arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<buckle_api::ErrorView>,
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
    /// How often the agent refused the approved Apply because its fresh plan
    /// differed (a non-deterministic plan); reset once an Apply is accepted
    /// or an operator re-plans.
    #[serde(default)]
    pub stale_refusals: u32,
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
    /// The `until` of the latest bake request: the agent samples no longer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake_asked_until: Option<DateTime<Utc>>,
    /// Modules the plan defers to apply time (their plan is unknown yet).
    #[serde(default)]
    pub deferred_modules: Vec<String>,
    /// The host was held at baseline (`baseline: hold`); if it was never
    /// applied (still held, or released by an operator's skip) it moves to
    /// the stragglers stage on promotion.
    #[serde(default)]
    pub held_at_baseline: bool,
    /// The intent digest of the host's current plan
    /// (`PlanResult.approval_digest`), from agents that report one. An
    /// approval binding it covers re-plans that differ only in content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_digest: Option<String>,
    /// An operator force-applied the host: the Apply binds no digest and the
    /// agent runs whatever it plans. Cleared by the result or a re-plan.
    #[serde(default)]
    pub force_plan: bool,
    /// The reboot the host's run asked for. Kept once the host is back, so
    /// that its bake checks the host after the reboot; cleared by the next
    /// apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot: Option<RebootWait>,
    /// A host, or a cluster applied through an executor.
    #[serde(default)]
    pub kind: TargetKind,
    /// The target's steps through its phase, in stage order (see
    /// [`super::stages`]). Empty means one whole step in stage 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// Index into `steps` of the step being worked on.
    #[serde(default)]
    pub step: usize,
    /// The stage of the current step (what the phase's wave logic gates on).
    #[serde(default)]
    pub stage: u32,
    /// For a cluster: the host agent its messages travel through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// The latest plan request went out before the target's stage opened:
    /// its plan is a preview, planned again when the stage opens.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
    /// The intent (else full) digest of the preview being re-planned, until
    /// the fresh plan arrives and is compared with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previewed: Option<String>,
    /// The preview digest the current plan drifted from: the plan changed
    /// between the rollout's start and its stage opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drifted_from: Option<String>,
    /// In an action rollout: the run of the action sent (its result's
    /// `action_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_run: Option<String>,
    /// A placement whose workload left the configuration: its plan removes
    /// what the workload owned on the cluster, once the placements that
    /// needed it (`after`) are done.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removal: bool,
    /// Placements of the same cluster that must finish before this one
    /// starts (a removal's dependents, by the applied revision's `needs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Plans the row applied before it was planned again (a retry, a force
    /// apply, its next step), oldest first, so that the rollout still shows
    /// everything it changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<PlanAttempt>,
}

/// A plan a row applied in an earlier attempt or step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAttempt {
    /// From 1.
    pub attempt: u32,
    #[serde(default)]
    pub stage: u32,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
    #[serde(default)]
    pub changes: Option<u32>,
    #[serde(default)]
    pub removals: Option<u32>,
    pub state: HostPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at: DateTime<Utc>,
}

impl HostRuntime {
    /// The step being worked on (a whole step when none were laid out).
    pub fn current_step(&self) -> Step {
        self.steps
            .get(self.step)
            .cloned()
            .unwrap_or_else(super::stages::whole)
    }

    /// The step after the current one, if any.
    pub fn next_step(&self) -> Option<&Step> {
        self.steps.get(self.step + 1)
    }

    /// Whether the plan of the current step waits for its stage to open: a
    /// cluster's or a placement's (it depends on the stages before it), or
    /// any later step.
    pub fn plan_deferred(&self) -> bool {
        self.kind.on_cluster() || self.step > 0
    }

    /// Lays the target out as `steps` and points it at the first.
    pub fn set_steps(&mut self, steps: Vec<Step>) {
        self.stage = steps.first().map_or(0, |s| s.stage);
        self.steps = steps;
        self.step = 0;
    }
}

/// A reboot a rollout waits out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebootWait {
    /// The boot the run happened in: a session from another boot is the
    /// host back. `None` from hosts that expose no boot id (any new session
    /// counts).
    #[serde(default)]
    pub boot_id: Option<String>,
    pub requested_by: Vec<String>,
    /// When the result announcing it arrived.
    pub since: DateTime<Utc>,
    /// When the host fails if it is not back.
    pub until: DateTime<Utc>,
    #[serde(default)]
    pub back_at: Option<DateTime<Utc>>,
}

impl RebootWait {
    /// Whether a session from `boot` is the host back from this reboot.
    pub fn back_on(&self, boot: Option<&str>) -> bool {
        match (self.boot_id.as_deref(), boot) {
            (Some(then), Some(now)) => then != now,
            _ => true,
        }
    }

    pub fn view(&self) -> buckle_api::HostReboot {
        buckle_api::HostReboot {
            requested_by: self.requested_by.clone(),
            since: self.since,
            until: self.until,
            back_at: self.back_at,
        }
    }
}

/// One `rollout_hosts` row: a host, a whole cluster (an executor that
/// predates placements), or one workload placement of a cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutHostRecord {
    pub rollout: String,
    /// The host, or the cluster a placement is on.
    pub host: String,
    /// The placement's workload; empty for a host's or a whole cluster's row.
    #[serde(default)]
    pub workload: String,
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
