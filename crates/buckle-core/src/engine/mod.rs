//! The engine: load a revision, collect facts, select modules, resolve vars and
//! secrets, plan, apply, remove, and run health checks.
//!
//! The pipeline has a pure half and a mutating half (design §04):
//!
//! * [`Engine::plan`] reads the host and produces a [`Plan`]. It never writes.
//! * [`Engine::apply`] re-plans, refuses when the digest no longer matches the
//!   plan it was handed (a stale plan), and then drives the host from the fresh
//!   plan: modules concurrently along the `needs` DAG, tasks in order, resource
//!   locks across modules, handlers, per-module health with settle, facts
//!   refresh and deferred re-planning, module-scoped rollback and halt, and
//!   finally removals of orphaned resources.
//! * [`Engine::health`] runs module health checks once for a scope.
//!
//! Internals are split into [`context`] (labels, groups, vars), [`graph`]
//! (module DAG), [`planner`], [`checks`] and [`executor`].

mod checks;
mod context;
mod executor;
mod graph;
pub mod loader;
mod planner;
pub mod targets;
pub mod validate;

#[cfg(test)]
mod tests;

pub use context::{layer_vars_over, merged_labels, render_vars};
pub use planner::action_step_key;
pub use targets::{selected_workloads, validate_targets};
pub use validate::{
    HostSummary, ValidationIssue, ValidationReport, cluster_context, host_context,
    placement_context, reads_unknown_facts, secret_names_for_cluster, secret_names_for_host,
    validate, validate_host, validate_with_facts, with_groups,
};

use crate::error::ErrorReport;
use crate::facts::Facts;
use crate::health::{HealthReport, HealthScope};
use crate::model::FleetConfig;
use crate::plan::{Plan, RevisionId};
use crate::secrets::SecretStore;
use crate::selectors::{HostIdentity, Labels, Vars};
use crate::state::StateStore;
use crate::system::System;
use crate::task::Source;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// A loaded revision: the fleet config plus every module and workload and
/// their files.
#[derive(Debug)]
pub struct Revision {
    pub id: RevisionId,
    pub fleet: FleetConfig,
    pub modules: Vec<LoadedModule>,
    /// The workloads under `workloads/`: what the revision's cluster targets
    /// run. Hosts never plan them; the server hands them to a cluster's
    /// executor.
    pub workloads: Vec<LoadedWorkload>,
    /// Raw contents of every `vars` overlay file referenced by the fleet config.
    pub var_files: indexmap::IndexMap<PathBuf, serde_yaml::Value>,
    /// Directories under `modules/` that have no `module.yaml`. Their module
    /// is indeterminate: resources recorded under that name are never treated
    /// as orphans, because the directory may be a module mid-edit.
    pub unloaded: Vec<String>,
    /// Directories under `workloads/` that have no `workload.yaml`.
    pub unloaded_workloads: Vec<String>,
}

impl Revision {
    /// The module with this name, if the revision has one.
    pub fn module(&self, name: &str) -> Option<&LoadedModule> {
        self.modules.iter().find(|m| m.config.name == name)
    }

    /// The workload with this name, if the revision has one.
    pub fn workload(&self, name: &str) -> Option<&LoadedWorkload> {
        self.workloads.iter().find(|w| w.config.name == name)
    }
}

/// One workload of a revision: its parsed `workload.yaml`, a reader for its
/// `files/` directory and the templates (`.j2`) found there.
pub struct LoadedWorkload {
    pub config: crate::model::WorkloadConfig,
    pub source: Arc<dyn Source>,
    /// Every file under `files/` by relative path, for validation (the specs
    /// a workload names must exist) and for templates.
    pub files: Vec<PathBuf>,
    /// Templates under `files/` keyed by relative path.
    pub templates: Vec<(PathBuf, String)>,
}

impl std::fmt::Debug for LoadedWorkload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedWorkload")
            .field("workload", &self.config.name)
            .field("files", &self.files.len())
            .finish()
    }
}

impl std::fmt::Debug for LoadedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedModule")
            .field("module", &self.config.name)
            .field("templates", &self.templates.len())
            .finish()
    }
}

/// One module of a revision: its parsed `module.yaml`, a reader for its
/// `files/` directory and the templates found there.
pub struct LoadedModule {
    pub config: crate::model::ModuleConfig,
    pub source: Arc<dyn Source>,
    /// Templates under `files/` keyed by relative path, for includes.
    pub templates: Vec<(PathBuf, String)>,
}

/// Inputs to a plan or apply for one host.
pub struct Inputs {
    pub revision: Arc<Revision>,
    pub host: HostIdentity,
    /// Labels managed on the server, overlaid on the agent's own.
    pub server_labels: Labels,
    pub system: Arc<dyn System>,
    pub secrets: Arc<SecretStore>,
    pub state: Arc<dyn StateStore>,
    /// Previously collected facts, refreshed per provider TTL.
    pub facts: Facts,
    /// Restrict to tasks matching this selector (e.g. `task.tags contains "network"`).
    pub select: Option<crate::Filter>,
    /// Checks waived for this run: `module/check`.
    pub waived_checks: Vec<String>,
    pub parallelism: usize,
    /// The last known-good revision, used to re-apply a module that fails
    /// (module-scoped rollback, design §04). `None` disables rollback.
    pub stable_revision: Option<Arc<Revision>>,
    /// Agent-local var overrides (`vars:` in agent.yaml); the last layer of the
    /// vars precedence (design §03).
    pub agent_vars: Vars,
    /// When set, `facts` is used as-is and no provider runs: for tests and
    /// `--facts` fixtures.
    pub facts_frozen: bool,
    /// Whether apply runs the plan's removals. When false the plan still lists
    /// them (and its digest is unchanged) but apply leaves the resources and
    /// their records alone: the agent's unattended drift correction passes
    /// false so that only an operator-approved apply removes anything.
    pub removals: bool,
    /// Drift mode: plan only the modules this host has applied at
    /// `revision` (per [`crate::state::State::module_at`]) and never treat a
    /// resource recorded at another revision as an orphan. Modules that moved
    /// on to a newer revision, failed, or never ran are reported in
    /// [`Plan::held`] and left alone. Set it on the inputs of both the plan
    /// and the apply (the apply re-plans and compares digests).
    pub drift: bool,
    /// Directories planning may write to besides the system's temp dir (the
    /// agent passes `<state_dir>/tmp`). Any other mutation while planning is
    /// refused (see [`crate::system::PlanGuard`]).
    pub scratch_dirs: Vec<PathBuf>,
    /// Cluster credentials for tasks that ask for one (`command.run`
    /// `identity`); `None` where no agent can issue them.
    pub clusters: Option<Arc<dyn crate::task::ClusterCredentials>>,
}

impl Inputs {
    /// Inputs with sensible defaults: no server labels, no cached facts, no
    /// selector, no waivers, parallelism from the CPU count, no stable revision.
    pub fn new(
        revision: Arc<Revision>,
        host: HostIdentity,
        system: Arc<dyn System>,
        secrets: Arc<SecretStore>,
        state: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            revision,
            host,
            server_labels: Labels::new(),
            system,
            secrets,
            state,
            facts: Facts::new(),
            select: None,
            waived_checks: Vec::new(),
            parallelism: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            stable_revision: None,
            agent_vars: Vars::new(),
            facts_frozen: false,
            removals: true,
            drift: false,
            scratch_dirs: Vec::new(),
            clusters: None,
        }
    }

    /// Cluster credentials for tasks that ask for one.
    pub fn with_clusters(
        mut self,
        clusters: Option<Arc<dyn crate::task::ClusterCredentials>>,
    ) -> Self {
        self.clusters = clusters;
        self
    }

    /// A directory planning may write to (e.g. `<state_dir>/tmp`).
    pub fn with_scratch_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.scratch_dirs.push(dir.into());
        self
    }

    /// Drift mode; see [`Inputs::drift`].
    pub fn with_drift(mut self, drift: bool) -> Self {
        self.drift = drift;
        self
    }

    /// Whether apply runs the plan's removals (default true). Plans list
    /// removals either way.
    pub fn with_removals(mut self, removals: bool) -> Self {
        self.removals = removals;
        self
    }

    /// Labels managed on the server, overlaid on the agent's own.
    pub fn with_server_labels(mut self, labels: Labels) -> Self {
        self.server_labels = labels;
        self
    }

    /// Seeds the facts cache; providers whose TTL expired still re-run.
    pub fn with_facts(mut self, facts: Facts) -> Self {
        self.facts = facts;
        self
    }

    /// Uses `facts` verbatim and runs no provider.
    pub fn with_frozen_facts(mut self, facts: Facts) -> Self {
        self.facts = facts;
        self.facts_frozen = true;
        self
    }

    /// Restricts the run to tasks matching a selector over the task scope.
    pub fn with_select(mut self, select: Option<crate::Filter>) -> Self {
        self.select = select;
        self
    }

    /// Health checks (`module/check`) that may fail without failing the run.
    pub fn with_waived_checks(mut self, waived: Vec<String>) -> Self {
        self.waived_checks = waived;
        self
    }

    /// How many modules may apply at once (at least one).
    pub fn with_parallelism(mut self, parallelism: usize) -> Self {
        self.parallelism = parallelism.max(1);
        self
    }

    /// The known-good revision a failing module is re-applied from.
    pub fn with_stable_revision(mut self, stable: Option<Arc<Revision>>) -> Self {
        self.stable_revision = stable;
        self
    }

    /// Agent-local var overrides, the last layer of the vars precedence.
    pub fn with_agent_vars(mut self, vars: Vars) -> Self {
        self.agent_vars = vars;
        self
    }

    /// The agent's labels with the server's overlaid (server wins).
    pub fn labels(&self) -> Labels {
        context::merged_labels(&self.host.labels, &self.server_labels)
    }
}

/// The cause [`Engine::apply`] attaches to its stale-plan error: the host now
/// plans differently from the plan it was handed (design §04 "a changed
/// system between plan and apply … surfaces as a stale plan error"). Callers
/// find it with [`StalePlan::find`] instead of matching the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalePlan {
    /// The digest of the plan handed to apply.
    pub planned: crate::Digest,
    /// The digest the host plans as now.
    pub fresh: crate::Digest,
}

impl std::fmt::Display for StalePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "planned {} but the host now plans {}",
            self.planned, self.fresh
        )
    }
}

impl std::error::Error for StalePlan {}

impl StalePlan {
    /// The stale-plan marker in `err`'s cause chain, if it is one.
    pub fn find(err: &crate::Error) -> Option<&StalePlan> {
        let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
        while let Some(e) = cause {
            if let Some(stale) = e.downcast_ref::<StalePlan>() {
                return Some(stale);
            }
            cause = e.source();
        }
        None
    }
}

/// Result of an apply.
///
/// The agent ships it to the server as `ApplyResult.outcome_json`, so every
/// field added after the first release is `#[serde(default)]`: a newer server
/// reads an older agent's outcome and the other way round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub plan_digest: crate::Digest,
    pub succeeded: bool,
    pub changes: usize,
    pub removals: usize,
    pub duration_ms: u64,
    pub module_results: Vec<ModuleOutcome>,
    /// The module whose failure halted the run, if any.
    pub failed_module: Option<String>,
    /// Modules re-applied from the stable revision because they failed.
    pub rolled_back_modules: Vec<String>,
    /// Modules not started because the run halted.
    pub not_started: Vec<String>,
    pub health: Option<HealthReport>,
    /// Every problem of the run on one line each, joined (no advice).
    pub error: Option<String>,
    /// The run-level error, structured: what failed, why, and what to try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorReport>,
    /// The removals of the plan (and of deferred modules), in order, with
    /// what happened to each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals_run: Vec<RemovalOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// A step or handler of the run asked for a reboot: whoever drove the run
    /// reboots the host once it has been reported. Health checks the run had
    /// not reached by then were left for after the reboot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot: Option<crate::reboot::RebootRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleOutcome {
    pub module: String,
    pub status: ModuleStatus,
    pub changes: usize,
    pub duration_ms: u64,
    /// The module's problem on one line (no advice).
    pub error: Option<String>,
    /// `error`, structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorReport>,
    /// The module's planned steps in plan order, with what happened to each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepOutcome>,
    /// The handlers the module ran (or planned and did not run), in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<StepOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// A deferred module not applied because its real plan diverged from
    /// the approved preview: the host needs re-planning, not repair.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

impl ModuleOutcome {
    /// An outcome with no steps, timings or error.
    pub fn new(module: impl Into<String>, status: ModuleStatus) -> Self {
        Self {
            module: module.into(),
            status,
            changes: 0,
            duration_ms: 0,
            error: None,
            error_detail: None,
            steps: Vec::new(),
            handlers: Vec::new(),
            started_at: None,
            finished_at: None,
            stale: false,
        }
    }

    /// Sets the error (both the line and the structured form).
    pub fn with_error(mut self, error: ErrorReport) -> Self {
        self.error = Some(error.one_line());
        self.error_detail = Some(error);
        self
    }
}

/// The state of one step (task, handler or removal) of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Planned, not started yet.
    Pending,
    Running,
    Changed,
    Unchanged,
    Failed,
    /// Could not be applied here (tool missing, platform, precondition).
    Blocked,
    /// Not run on purpose (a handler nothing notified, removals disabled).
    Skipped,
    /// Never reached because the module or the run halted.
    NotRun,
    /// Applied, then undone by the module's rollback to the stable revision.
    RolledBack,
}

/// What happened to one planned step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    /// The resource key within the module, `kind[key]` (for a handler,
    /// `owner:handler`).
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the plan said the step would do.
    pub summary: String,
    pub state: StepState,
    /// What applying it reported, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorReport>,
}

impl StepOutcome {
    /// The not-yet-run outcome of a planned step.
    pub fn planned(step: &crate::plan::Step, state: StepState) -> Self {
        Self {
            id: step_id(&step.resource),
            kind: step.resource.kind.clone(),
            name: step.task.name.clone(),
            summary: planned_summary(&step.planned),
            state,
            result: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            error_detail: None,
        }
    }

    /// The not-yet-run outcome of a planned handler.
    pub fn planned_handler(run: &crate::plan::HandlerRun, state: StepState) -> Self {
        Self {
            id: handler_id(&run.module, &run.handler),
            kind: run.kind.clone(),
            name: Some(run.handler.clone()),
            summary: run.summary.clone(),
            state,
            result: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            error_detail: None,
        }
    }
}

/// The id of a step within its module: `kind[key]`.
pub fn step_id(resource: &crate::plan::ResourceKey) -> String {
    format!("{}[{}]", resource.kind, resource.key)
}

/// The id of a handler: `owner:handler`.
pub fn handler_id(owner: &str, handler: &str) -> String {
    format!("{owner}:{handler}")
}

/// What a plan said a step would do, in one line.
pub fn planned_summary(planned: &crate::plan::Planned) -> String {
    match planned {
        crate::plan::Planned::NoOp => "no change".to_owned(),
        crate::plan::Planned::Change { diff } => diff.summary().to_owned(),
        crate::plan::Planned::Blocked { reason } => format!("blocked: {reason}"),
    }
}

/// What happened to one removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalOutcome {
    /// `module/kind[key]`.
    pub resource: String,
    pub kind: String,
    /// What the plan said the removal would do; once it ran, what it did.
    pub summary: String,
    pub state: StepState,
    #[serde(default)]
    pub caution: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleStatus {
    Applied,
    Unchanged,
    Failed,
    Unhealthy,
    RolledBack,
    NotStarted,
    Deferred,
    /// The module's selection could not be decided on this host (see
    /// [`crate::plan::Indeterminate`]); it was not applied.
    Indeterminate,
    /// The module applied what it could but some steps stayed blocked; the
    /// run needs attention (it does not count as a success).
    Blocked,
    /// The module was deferred, and its `when` turned out false against the
    /// refreshed facts: not applied, its resources removed as orphans.
    Skipped,
}

/// Progress events emitted during apply, for the agent to stream to the server.
///
/// Step events carry `id`, the step's resource key within the module
/// (`kind[key]`, or `owner:handler` for a handler), so that they can be matched
/// to the plan's steps. Fields added after the first release are
/// `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ApplyEvent {
    ModuleStarted {
        module: String,
    },
    StepStarted {
        module: String,
        task_id: String,
        #[serde(default)]
        id: String,
        kind: String,
    },
    StepFinished {
        module: String,
        task_id: String,
        #[serde(default)]
        id: String,
        kind: String,
        changed: bool,
        summary: String,
    },
    StepFailed {
        module: String,
        task_id: String,
        #[serde(default)]
        id: String,
        kind: String,
        /// The one-line message (no causes or advice).
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_detail: Option<ErrorReport>,
    },
    /// A step that could not be applied here (it stays planned).
    StepBlocked {
        module: String,
        task_id: String,
        id: String,
        kind: String,
        reason: String,
    },
    /// A step deliberately not run (a handler nothing notified).
    StepSkipped {
        module: String,
        task_id: String,
        id: String,
        kind: String,
        reason: String,
    },
    /// A step still running says how far it got (a deployment's healthy
    /// allocations). Never terminal: a later message replaces it.
    StepProgress {
        module: String,
        task_id: String,
        id: String,
        kind: String,
        message: String,
    },
    HandlerRun {
        module: String,
        handler: String,
    },
    ModuleFinished {
        module: String,
        status: ModuleStatus,
    },
    RemovalRun {
        resource: String,
        summary: String,
    },
    HealthChecked {
        module: String,
        check: String,
        passed: bool,
        reason: Option<String>,
    },
    FactsRefreshed {
        providers: Vec<String>,
    },
}

impl ApplyEvent {
    /// The module and step id the event is about, when it is about a step.
    pub fn step(&self) -> Option<(&str, &str)> {
        match self {
            ApplyEvent::StepStarted { module, id, .. }
            | ApplyEvent::StepFinished { module, id, .. }
            | ApplyEvent::StepFailed { module, id, .. }
            | ApplyEvent::StepBlocked { module, id, .. }
            | ApplyEvent::StepSkipped { module, id, .. }
            | ApplyEvent::StepProgress { module, id, .. } => Some((module, id)),
            _ => None,
        }
    }

    /// The step state the event leaves its step in, when it is about a step.
    pub fn step_state(&self) -> Option<StepState> {
        Some(match self {
            ApplyEvent::StepStarted { .. } => StepState::Running,
            ApplyEvent::StepFinished { changed: true, .. } => StepState::Changed,
            ApplyEvent::StepFinished { .. } => StepState::Unchanged,
            ApplyEvent::StepFailed { .. } => StepState::Failed,
            ApplyEvent::StepBlocked { .. } => StepState::Blocked,
            ApplyEvent::StepSkipped { .. } => StepState::Skipped,
            _ => return None,
        })
    }

    /// Whether the event ends its step (or module): the event a coalescing
    /// transport must never drop.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ApplyEvent::StepFinished { .. }
                | ApplyEvent::StepFailed { .. }
                | ApplyEvent::StepBlocked { .. }
                | ApplyEvent::StepSkipped { .. }
                | ApplyEvent::ModuleFinished { .. }
                | ApplyEvent::RemovalRun { .. }
        )
    }
}

/// The engine entry points.
pub struct Engine;

impl Engine {
    /// Pure: collects facts (respecting TTLs), selects modules, resolves vars,
    /// plans every task and removal. Never mutates the host.
    pub async fn plan(inputs: &Inputs) -> crate::Result<(Plan, Facts)> {
        let span = tracing::info_span!("engine.plan", buckle.host = %inputs.host.id, buckle.revision = %inputs.revision.id);
        let (rich, facts) =
            tracing::Instrument::instrument(planner::plan(inputs, &inputs.revision), span).await?;
        Ok((rich.plan, facts))
    }

    /// [`Engine::plan`] in drift mode (see [`Inputs::drift`]) whatever
    /// `inputs.drift` says. To correct the drift, apply the plan with inputs
    /// that have [`Inputs::with_drift`]`(true)` (and, unattended,
    /// [`Inputs::with_removals`]`(false)`).
    pub async fn drift(inputs: &Inputs) -> crate::Result<(Plan, Facts)> {
        let span = tracing::info_span!("engine.drift", buckle.host = %inputs.host.id, buckle.revision = %inputs.revision.id);
        let (rich, facts) = tracing::Instrument::instrument(
            planner::plan_mode(inputs, &inputs.revision, true),
            span,
        )
        .await?;
        Ok((rich.plan, facts))
    }

    /// Applies a previously produced plan. Refuses when the host's fresh plan
    /// digest no longer matches (stale plan).
    pub async fn apply(
        inputs: &Inputs,
        plan: &Plan,
        events: tokio::sync::mpsc::Sender<ApplyEvent>,
    ) -> crate::Result<ApplyOutcome> {
        let span = tracing::info_span!("engine.apply", buckle.host = %inputs.host.id, buckle.revision = %inputs.revision.id, buckle.plan.digest = %plan.digest.short());
        tracing::Instrument::instrument(executor::apply(inputs, plan, events), span).await
    }

    /// Pure: plans `action` of `module` on this host (a plan whose only
    /// module is `module`, holding the action's step and the handlers it
    /// notifies). Fails when the module is not selected here, declares no
    /// such action, or the action's `when` is false.
    pub async fn plan_action(
        inputs: &Inputs,
        module: &str,
        action: &str,
    ) -> crate::Result<(Plan, Facts)> {
        let span = tracing::info_span!("engine.plan_action", buckle.host = %inputs.host.id, buckle.revision = %inputs.revision.id, buckle.module = %module, buckle.action = %action);
        tracing::Instrument::instrument(
            async {
                let planner = planner::Planner::from_inputs(inputs, &inputs.revision)?;
                let facts = planner::collect_facts(inputs, &planner).await;
                let state = inputs.state.load()?;
                let (rich, warnings) =
                    planner::plan_action_with(&planner, &facts, &state, module, action).await?;
                Ok((planner::action_plan(&planner, &rich, warnings), facts))
            },
            span,
        )
        .await
    }

    /// Runs `action` of `module` as planned by [`Engine::plan_action`]
    /// (refusing a stale plan, as [`Engine::apply`] does): its step, the
    /// handlers it notifies and, when it changed something, the module's
    /// health checks. Nothing is recorded in state; nothing is rolled back.
    pub async fn run_action(
        inputs: &Inputs,
        module: &str,
        action: &str,
        plan: &Plan,
        events: tokio::sync::mpsc::Sender<ApplyEvent>,
    ) -> crate::Result<ApplyOutcome> {
        let span = tracing::info_span!("engine.run_action", buckle.host = %inputs.host.id, buckle.revision = %inputs.revision.id, buckle.module = %module, buckle.action = %action, buckle.plan.digest = %plan.digest.short());
        tracing::Instrument::instrument(
            executor::apply_action(inputs, module, action, plan, events),
            span,
        )
        .await
    }

    /// Runs the health checks of the given modules (or all modules selected on
    /// this host when `None`) once, with the given scope.
    pub async fn health(
        inputs: &Inputs,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> crate::Result<HealthReport> {
        checks::run_scope(inputs, scope, modules).await
    }
}
