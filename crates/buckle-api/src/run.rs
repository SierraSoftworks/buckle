//! Per-host run reports: the plan a host follows, module by module and step by
//! step, with the state of each one. The same shape serves a run that is in
//! progress (states `pending` and `running` from the live apply events) and a
//! finished one (states from the agent's result), so the UI renders both the
//! same way and errors arrive structured rather than as one flattened string.

use crate::fleet::HealthResultDto;
use crate::plan::DiffKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a run ended, or `pending`/`running` while it has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// The host has a plan but has not started applying it.
    Pending,
    Running,
    Succeeded,
    /// A module failed; failed modules were rolled back where a stable
    /// revision was available.
    Failed,
    /// Nothing failed outright but steps stayed blocked; the revision is not
    /// fully applied and an operator must look.
    NeedsAttention,
    /// The whole host was returned to its stable revision.
    RolledBack,
    /// The host re-planned differently from the approved plan and refused it.
    StalePlan,
}

/// The state of one module within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleState {
    Pending,
    Running,
    Applied,
    Unchanged,
    Failed,
    /// Applied, but a required health check failed after the settle window.
    Unhealthy,
    RolledBack,
    /// Not started because the run halted on an earlier module.
    NotStarted,
    /// Waiting for facts a provider module produces; planned again later.
    Deferred,
    /// Its selection could not be decided on this host; never applied.
    Indeterminate,
    /// Applied what it could but some steps stayed blocked.
    Blocked,
    /// Deferred, then found unselected once the facts arrived.
    Skipped,
}

/// The state of one step (a task, handler or removal) within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Planned, not started yet.
    Pending,
    Running,
    Changed,
    Unchanged,
    Failed,
    /// Could not be applied (tool missing, platform, precondition); planned
    /// again later or left for an operator.
    Blocked,
    /// Skipped by the agent (for example a step whose `when` turned false).
    Skipped,
    /// Never reached because an earlier step or module failed.
    NotRun,
    /// Undone by a module rollback to the stable revision.
    RolledBack,
}

/// Who should act on an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Something the configuration author or operator can fix.
    User,
    /// Something in Buckle or the host that needs investigation.
    System,
}

/// A structured error: what went wrong, the chain of causes beneath it and
/// the advice attached to it, so a reader never has to parse a dumped string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorView {
    pub kind: ErrorKind,
    /// The top-level description, one sentence.
    pub message: String,
    /// Underlying causes, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// What to try, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

/// One step of a module: a task, or a handler when `handler` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReport {
    /// The resource key (`kind[key]`), unique within the module.
    pub id: String,
    pub kind: String,
    /// The task's `name`, when the author gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the plan said the step would do.
    pub summary: String,
    pub state: StepState,
    /// What applying it reported, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffKind>,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub handler: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorView>,
}

/// One module of the run with its steps in plan order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleReport {
    pub name: String,
    pub state: ModuleState,
    /// Why the module is deferred, indeterminate or skipped, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Steps that changed something.
    #[serde(default)]
    pub changes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorView>,
    pub steps: Vec<StepReport>,
    /// Handlers notified by the steps, in the order they ran (or will run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<StepReport>,
    /// Health checks run for this module in this run (settle and bake).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health: Vec<HealthResultDto>,
}

/// A resource removed because the revision no longer declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalReport {
    /// `module/kind[key]`.
    pub resource: String,
    pub kind: String,
    pub summary: String,
    pub state: StepState,
    #[serde(default)]
    pub caution: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorView>,
}

/// Module counts by state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleCounts {
    pub pending: u32,
    pub running: u32,
    pub applied: u32,
    pub unchanged: u32,
    pub failed: u32,
    pub rolled_back: u32,
    pub not_started: u32,
    pub blocked: u32,
    pub indeterminate: u32,
    pub skipped: u32,
}

/// Step counts by state (tasks, handlers and removals together).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCounts {
    pub pending: u32,
    pub running: u32,
    pub changed: u32,
    pub unchanged: u32,
    pub failed: u32,
    pub blocked: u32,
    pub skipped: u32,
    pub not_run: u32,
    pub rolled_back: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCounts {
    pub modules: ModuleCounts,
    pub steps: StepCounts,
}

/// The full report of one run on one host.
///
/// `GET /api/v1/hosts/{id}/runs/{run}` serves it for any recorded run,
/// `HostDetail.latest_run` carries the newest one, and
/// `GET /api/v1/rollouts/{id}/hosts/{host}/report` serves the host's run for
/// that rollout, live while it is applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Stable identifier of the run (`RunSummary.id`).
    pub id: String,
    pub host: String,
    pub revision: String,
    pub plan_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// When the report was last updated (the latest event for a live run).
    pub updated_at: DateTime<Utc>,
    pub counts: RunCounts,
    pub modules: Vec<ModuleReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<RemovalReport>,
    /// A run-level error (stale plan, state could not be saved, …) as opposed
    /// to a module's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
    /// For an operator's action, `module/action`; absent for an apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// A live update to one step or module of a run in progress; the UI applies it
/// to the report it holds, or refetches the report when it has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProgress {
    pub host: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    pub at: DateTime<Utc>,
    /// The run's state after this update.
    pub run_state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_state: Option<ModuleState>,
    /// The step's `id` within the module, when the update is about a step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_state: Option<StepState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorView>,
}

impl RunReport {
    /// Recomputes `counts` from the modules, steps, handlers and removals.
    pub fn recount(&mut self) {
        let mut counts = RunCounts::default();
        for module in &self.modules {
            let m = &mut counts.modules;
            match module.state {
                ModuleState::Pending => m.pending += 1,
                ModuleState::Running => m.running += 1,
                ModuleState::Applied => m.applied += 1,
                ModuleState::Unchanged => m.unchanged += 1,
                ModuleState::Failed | ModuleState::Unhealthy => m.failed += 1,
                ModuleState::RolledBack => m.rolled_back += 1,
                ModuleState::NotStarted => m.not_started += 1,
                ModuleState::Deferred => m.pending += 1,
                ModuleState::Indeterminate => m.indeterminate += 1,
                ModuleState::Blocked => m.blocked += 1,
                ModuleState::Skipped => m.skipped += 1,
            }
            for step in module.steps.iter().chain(module.handlers.iter()) {
                counts.steps.add(step.state);
            }
        }
        for removal in &self.removals {
            counts.steps.add(removal.state);
        }
        self.counts = counts;
    }

    /// Modules that need a reader's attention, in plan order.
    pub fn problem_modules(&self) -> impl Iterator<Item = &ModuleReport> {
        self.modules.iter().filter(|m| {
            matches!(
                m.state,
                ModuleState::Failed
                    | ModuleState::Unhealthy
                    | ModuleState::RolledBack
                    | ModuleState::Blocked
                    | ModuleState::Indeterminate
            )
        })
    }
}

impl StepCounts {
    fn add(&mut self, state: StepState) {
        match state {
            StepState::Pending => self.pending += 1,
            StepState::Running => self.running += 1,
            StepState::Changed => self.changed += 1,
            StepState::Unchanged => self.unchanged += 1,
            StepState::Failed => self.failed += 1,
            StepState::Blocked => self.blocked += 1,
            StepState::Skipped => self.skipped += 1,
            StepState::NotRun => self.not_run += 1,
            StepState::RolledBack => self.rolled_back += 1,
        }
    }
}

impl ErrorView {
    /// A user-facing error with no causes or advice.
    pub fn user(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::User,
            message: message.into(),
            causes: Vec::new(),
            advice: Vec::new(),
        }
    }

    /// A system error with no causes or advice.
    pub fn system(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::System,
            message: message.into(),
            causes: Vec::new(),
            advice: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn step(id: &str, state: StepState) -> StepReport {
        StepReport {
            id: id.into(),
            kind: "file.copy".into(),
            name: None,
            summary: format!("copy {id}"),
            state,
            result: None,
            diff: Some(DiffKind::Update),
            destructive: false,
            handler: false,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            error: None,
        }
    }

    fn report() -> RunReport {
        RunReport {
            id: "run-1".into(),
            host: "pi-1".into(),
            revision: "abc".into(),
            plan_digest: "d1".into(),
            rollout_id: None,
            state: RunState::Running,
            started_at: None,
            finished_at: None,
            updated_at: Utc::now(),
            counts: RunCounts::default(),
            modules: vec![
                ModuleReport {
                    name: "packages".into(),
                    state: ModuleState::Applied,
                    note: None,
                    started_at: None,
                    finished_at: None,
                    duration_ms: None,
                    changes: 1,
                    error: None,
                    steps: vec![
                        step("a", StepState::Changed),
                        step("b", StepState::Unchanged),
                    ],
                    handlers: vec![StepReport {
                        handler: true,
                        ..step("restart", StepState::Changed)
                    }],
                    health: vec![],
                },
                ModuleReport {
                    name: "nomad".into(),
                    state: ModuleState::Running,
                    note: None,
                    started_at: None,
                    finished_at: None,
                    duration_ms: None,
                    changes: 0,
                    error: None,
                    steps: vec![step("c", StepState::Running), step("d", StepState::Pending)],
                    handlers: vec![],
                    health: vec![],
                },
                ModuleReport {
                    name: "swap".into(),
                    state: ModuleState::Failed,
                    note: None,
                    started_at: None,
                    finished_at: None,
                    duration_ms: None,
                    changes: 0,
                    error: Some(ErrorView::user("mkswap failed")),
                    steps: vec![step("e", StepState::Failed), step("f", StepState::NotRun)],
                    handlers: vec![],
                    health: vec![],
                },
            ],
            removals: vec![RemovalReport {
                resource: "old/file.copy[/etc/x]".into(),
                kind: "file.copy".into(),
                summary: "delete /etc/x".into(),
                state: StepState::Pending,
                caution: false,
                error: None,
            }],
            error: None,
            trace_id: None,
            trace_url: None,
            action: None,
        }
    }

    #[test]
    fn recount_tallies_modules_steps_handlers_and_removals() {
        let mut r = report();
        r.recount();
        assert_eq!(r.counts.modules.applied, 1);
        assert_eq!(r.counts.modules.running, 1);
        assert_eq!(r.counts.modules.failed, 1);
        assert_eq!(r.counts.steps.changed, 2, "a step and a handler changed");
        assert_eq!(r.counts.steps.unchanged, 1);
        assert_eq!(r.counts.steps.running, 1);
        assert_eq!(r.counts.steps.pending, 2, "a planned step and a removal");
        assert_eq!(r.counts.steps.failed, 1);
        assert_eq!(r.counts.steps.not_run, 1);
    }

    #[test]
    fn problem_modules_lists_only_what_needs_attention() {
        let report = report();
        let names: Vec<&str> = report.problem_modules().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["swap"]);
    }

    #[rstest]
    #[case(RunState::NeedsAttention, "\"needs_attention\"")]
    #[case(RunState::StalePlan, "\"stale_plan\"")]
    fn states_serialise_in_snake_case(#[case] state: RunState, #[case] json: &str) {
        assert_eq!(serde_json::to_string(&state).unwrap(), json);
        assert_eq!(serde_json::from_str::<RunState>(json).unwrap(), state);
    }

    #[test]
    fn optional_fields_are_omitted_and_tolerated() {
        let json = serde_json::to_value(ErrorView::user("boom")).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "user", "message": "boom"}));
        let parsed: StepReport = serde_json::from_value(serde_json::json!({
            "id": "x", "kind": "file.copy", "summary": "s", "state": "pending"
        }))
        .unwrap();
        assert_eq!(parsed.state, StepState::Pending);
        assert!(!parsed.handler);
    }
}
