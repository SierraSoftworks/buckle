//! Run reports: a host's run, module by module and step by step, built from
//! the plan it follows, the apply events seen so far and (once it arrived)
//! the agent's `ApplyOutcome`, which is authoritative.
//!
//! * Before any event every planned step is `pending`, and so is its module.
//! * While applying, states come from the events: the step that started and
//!   has not finished is `running`, and so is its module.
//! * Once the result arrives, states come from the outcome; what the outcome
//!   does not cover (an older agent's) keeps what the events said.

use crate::store::{RunEvent, RunRecord};
use buckle_api::{
    DiffKind, ErrorKind, ErrorView, HealthResultDto, ModuleReport, ModuleState, RemovalReport,
    RunCounts, RunProgress, RunReport, RunState, StepReport, StepState,
};
use buckle_core::engine::{self, ApplyEvent, ApplyOutcome, ModuleStatus};
use buckle_core::error::ErrorReport;
use buckle_core::plan::{Diff, Plan, Planned};
use chrono::{DateTime, Utc};

/// The API form of a structured error.
pub fn error_view(report: &ErrorReport) -> ErrorView {
    ErrorView {
        kind: match report.kind {
            buckle_core::error::ErrorKind::User => ErrorKind::User,
            buckle_core::error::ErrorKind::System => ErrorKind::System,
        },
        message: report.message.clone(),
        causes: report.causes.clone(),
        advice: report.advice.clone(),
    }
}

/// The error a reader should see first for an outcome: the run-level one,
/// else the first failing module's.
pub fn outcome_error(outcome: &ApplyOutcome) -> Option<ErrorView> {
    outcome.error_detail.as_ref().map(error_view).or_else(|| {
        outcome
            .module_results
            .iter()
            .filter(|m| {
                matches!(
                    m.status,
                    ModuleStatus::Failed | ModuleStatus::Unhealthy | ModuleStatus::RolledBack
                )
            })
            .find_map(|m| m.error_detail.as_ref().map(error_view))
    })
}

/// The report's run state for a stored run (`events` tells a run that has
/// not started from one that is applying).
pub fn run_state(run: &RunRecord, has_events: bool) -> RunState {
    match run.outcome.as_str() {
        crate::store::RUNNING if has_events => RunState::Running,
        crate::store::RUNNING => RunState::Pending,
        "succeeded" => RunState::Succeeded,
        "needs_attention" => RunState::NeedsAttention,
        "stale_plan" => RunState::StalePlan,
        "rolled_back" => RunState::RolledBack,
        _ => RunState::Failed,
    }
}

pub fn module_state(status: ModuleStatus) -> ModuleState {
    match status {
        ModuleStatus::Applied => ModuleState::Applied,
        ModuleStatus::Unchanged => ModuleState::Unchanged,
        ModuleStatus::Failed => ModuleState::Failed,
        ModuleStatus::Unhealthy => ModuleState::Unhealthy,
        ModuleStatus::RolledBack => ModuleState::RolledBack,
        ModuleStatus::NotStarted => ModuleState::NotStarted,
        ModuleStatus::Deferred => ModuleState::Deferred,
        ModuleStatus::Indeterminate => ModuleState::Indeterminate,
        ModuleStatus::Blocked => ModuleState::Blocked,
        ModuleStatus::Skipped => ModuleState::Skipped,
    }
}

pub fn step_state(state: engine::StepState) -> StepState {
    match state {
        engine::StepState::Pending => StepState::Pending,
        engine::StepState::Running => StepState::Running,
        engine::StepState::Changed => StepState::Changed,
        engine::StepState::Unchanged => StepState::Unchanged,
        engine::StepState::Failed => StepState::Failed,
        engine::StepState::Blocked => StepState::Blocked,
        engine::StepState::Skipped => StepState::Skipped,
        engine::StepState::NotRun => StepState::NotRun,
        engine::StepState::RolledBack => StepState::RolledBack,
    }
}

fn diff_kind(diff: &Diff) -> DiffKind {
    match diff {
        Diff::Create { .. } => DiffKind::Create,
        Diff::Update { .. } => DiffKind::Update,
        Diff::Delete { .. } => DiffKind::Delete,
        Diff::Opaque { .. } => DiffKind::Opaque,
    }
}

fn pending_module(name: &str, state: ModuleState) -> ModuleReport {
    ModuleReport {
        name: name.to_owned(),
        state,
        note: None,
        started_at: None,
        finished_at: None,
        duration_ms: None,
        changes: 0,
        error: None,
        steps: Vec::new(),
        handlers: Vec::new(),
        health: Vec::new(),
    }
}

fn bare_step(id: &str, kind: &str, handler: bool) -> StepReport {
    StepReport {
        id: id.to_owned(),
        kind: kind.to_owned(),
        name: None,
        summary: id.to_owned(),
        state: StepState::Pending,
        result: None,
        diff: None,
        destructive: false,
        handler,
        started_at: None,
        finished_at: None,
        duration_ms: None,
        error: None,
    }
}

/// The report skeleton of a plan: every module and step `pending`.
fn from_plan(report: &mut RunReport, plan: &Plan) {
    for m in &plan.modules {
        let mut module = pending_module(&m.module, ModuleState::Pending);
        module.steps = m
            .steps
            .iter()
            .map(|s| {
                let (diff, destructive) = match &s.planned {
                    Planned::Change { diff } => (Some(diff_kind(diff)), diff.is_destructive()),
                    _ => (None, false),
                };
                StepReport {
                    name: s.task.name.clone(),
                    summary: engine::planned_summary(&s.planned),
                    diff,
                    destructive,
                    ..bare_step(&engine::step_id(&s.resource), &s.resource.kind, false)
                }
            })
            .collect();
        module.handlers = m
            .handlers
            .iter()
            .map(|h| StepReport {
                name: Some(h.handler.clone()),
                summary: h.summary.clone(),
                ..bare_step(&engine::handler_id(&h.module, &h.handler), &h.kind, true)
            })
            .collect();
        report.modules.push(module);
    }
    for d in &plan.deferred {
        let mut module = pending_module(&d.module, ModuleState::Deferred);
        module.note = Some(d.reason.clone());
        report.modules.push(module);
    }
    for i in &plan.indeterminate {
        let mut module = pending_module(&i.module, ModuleState::Indeterminate);
        module.note = Some(i.reason.clone());
        if i.plan_error {
            module.error = Some(
                i.error_detail
                    .as_ref()
                    .map(error_view)
                    .unwrap_or_else(|| ErrorView::user(i.reason.clone())),
            );
        }
        report.modules.push(module);
    }
    report.removals = plan
        .removals
        .iter()
        .map(|r| RemovalReport {
            resource: r.resource.to_string(),
            kind: r.resource.kind.clone(),
            summary: r.summary.clone(),
            state: StepState::Pending,
            caution: r.caution,
            error: None,
        })
        .collect();
}

fn module_mut<'a>(report: &'a mut RunReport, name: &str) -> &'a mut ModuleReport {
    let index = match report.modules.iter().position(|m| m.name == name) {
        Some(i) => i,
        None => {
            report
                .modules
                .push(pending_module(name, ModuleState::Pending));
            report.modules.len() - 1
        }
    };
    &mut report.modules[index]
}

/// The step `id` of `module` (a task or a handler), added when the plan did
/// not list it (no plan, or a step planned at apply time).
fn step_mut<'a>(module: &'a mut ModuleReport, id: &str, kind: &str) -> &'a mut StepReport {
    if let Some(i) = module.steps.iter().position(|s| s.id == id) {
        return &mut module.steps[i];
    }
    if let Some(i) = module.handlers.iter().position(|s| s.id == id) {
        return &mut module.handlers[i];
    }
    // `owner:handler` ids are handlers; `kind[key]` ids are tasks.
    let handler = !id.ends_with(']') && id.contains(':');
    let list = if handler {
        &mut module.handlers
    } else {
        &mut module.steps
    };
    list.push(bare_step(id, kind, handler));
    list.last_mut().expect("just pushed")
}

fn finish_step(step: &mut StepReport, state: StepState, at: DateTime<Utc>) {
    step.state = state;
    step.finished_at = Some(at);
    if let Some(started) = step.started_at {
        step.duration_ms = Some((at - started).num_milliseconds().max(0) as u64);
    }
}

fn legacy_error(error: &str) -> ErrorView {
    ErrorView::system(error.to_owned())
}

/// Applies one live event to the report. `step_ids` says the agent sends
/// step ids (and so reports handlers with their own step events).
fn apply_event(report: &mut RunReport, event: &ApplyEvent, at: DateTime<Utc>, step_ids: bool) {
    match event {
        ApplyEvent::ModuleStarted { module } => {
            let m = module_mut(report, module);
            m.state = ModuleState::Running;
            m.started_at = Some(at);
        }
        ApplyEvent::ModuleFinished { module, status } => {
            let m = module_mut(report, module);
            m.state = module_state(*status);
            m.finished_at = Some(at);
            if let Some(started) = m.started_at {
                m.duration_ms = Some((at - started).num_milliseconds().max(0) as u64);
            }
            if matches!(status, ModuleStatus::Failed | ModuleStatus::Unhealthy) {
                for s in m.steps.iter_mut().chain(m.handlers.iter_mut()) {
                    if matches!(s.state, StepState::Pending | StepState::Running) {
                        s.state = StepState::NotRun;
                    }
                }
            }
        }
        ApplyEvent::StepStarted {
            module,
            id,
            kind,
            task_id,
            ..
        } => {
            let id = if id.is_empty() { task_id } else { id };
            let m = module_mut(report, module);
            if m.state == ModuleState::Pending {
                m.state = ModuleState::Running;
            }
            let s = step_mut(m, id, kind);
            s.state = StepState::Running;
            s.started_at = Some(at);
        }
        ApplyEvent::StepFinished {
            module,
            id,
            task_id,
            kind,
            changed,
            summary,
        } => {
            let id = if id.is_empty() { task_id } else { id };
            let s = step_mut(module_mut(report, module), id, kind);
            s.result = Some(summary.clone());
            finish_step(
                s,
                if *changed {
                    StepState::Changed
                } else {
                    StepState::Unchanged
                },
                at,
            );
        }
        ApplyEvent::StepFailed {
            module,
            id,
            task_id,
            kind,
            error,
            error_detail,
        } => {
            let id = if id.is_empty() { task_id } else { id };
            let s = step_mut(module_mut(report, module), id, kind);
            s.error = Some(
                error_detail
                    .as_ref()
                    .map(error_view)
                    .unwrap_or_else(|| legacy_error(error)),
            );
            finish_step(s, StepState::Failed, at);
        }
        ApplyEvent::StepBlocked {
            module,
            id,
            kind,
            reason,
            ..
        } => {
            let s = step_mut(module_mut(report, module), id, kind);
            s.result = Some(format!("blocked: {reason}"));
            finish_step(s, StepState::Blocked, at);
        }
        ApplyEvent::StepSkipped {
            module,
            id,
            kind,
            reason,
            ..
        } => {
            let s = step_mut(module_mut(report, module), id, kind);
            s.result = Some(reason.clone());
            finish_step(s, StepState::Skipped, at);
        }
        ApplyEvent::HandlerRun { module, handler } => {
            // Older agents report a handler only once it ran (a change).
            // Newer ones send StepStarted/StepFinished for a handler that
            // changes something, so a HandlerRun on a handler still pending
            // is one whose plan was a no-op.
            let id = engine::handler_id(module, handler);
            for m in &mut report.modules {
                if let Some(h) = m.handlers.iter_mut().find(|h| h.id == id) {
                    match h.state {
                        StepState::Pending if step_ids => {
                            h.result = Some("no change".to_owned());
                            finish_step(h, StepState::Unchanged, at);
                        }
                        StepState::Pending | StepState::Running => {
                            finish_step(h, StepState::Changed, at)
                        }
                        _ => {}
                    }
                }
            }
        }
        ApplyEvent::RemovalRun { resource, summary } => {
            let index = match report.removals.iter().position(|r| &r.resource == resource) {
                Some(i) => i,
                None => {
                    report.removals.push(RemovalReport {
                        resource: resource.clone(),
                        kind: resource
                            .split_once('/')
                            .and_then(|(_, rest)| rest.split_once('['))
                            .map(|(kind, _)| kind.to_owned())
                            .unwrap_or_default(),
                        summary: summary.clone(),
                        state: StepState::Pending,
                        caution: false,
                        error: None,
                    });
                    report.removals.len() - 1
                }
            };
            let r = &mut report.removals[index];
            match summary.strip_prefix("failed: ") {
                Some(message) => {
                    r.state = StepState::Failed;
                    r.error = Some(legacy_error(message));
                }
                None => {
                    r.state = StepState::Changed;
                    r.summary = summary.clone();
                }
            }
        }
        // While it runs, a step's result is how far it got.
        ApplyEvent::StepProgress {
            module,
            id,
            task_id,
            kind,
            message,
        } => {
            let id = if id.is_empty() { task_id } else { id };
            let s = step_mut(module_mut(report, module), id, kind);
            if matches!(s.state, StepState::Pending | StepState::Running) {
                s.state = StepState::Running;
                s.started_at.get_or_insert(at);
                s.result = Some(message.clone());
            }
        }
        ApplyEvent::HealthChecked { .. } | ApplyEvent::FactsRefreshed { .. } => {}
    }
}

/// Overlays the agent's outcome, which is authoritative.
fn apply_outcome(report: &mut RunReport, outcome: &ApplyOutcome) {
    for mo in &outcome.module_results {
        let m = module_mut(report, &mo.module);
        m.state = module_state(mo.status);
        m.duration_ms = Some(mo.duration_ms);
        m.started_at = mo.started_at.or(m.started_at);
        m.finished_at = mo.finished_at.or(m.finished_at);
        m.error = mo
            .error_detail
            .as_ref()
            .map(error_view)
            .or_else(|| mo.error.as_deref().map(legacy_error));
        if matches!(
            mo.status,
            ModuleStatus::Indeterminate | ModuleStatus::Deferred | ModuleStatus::Skipped
        ) {
            // Why it did not apply is a note, not an error.
            m.note = mo.error.clone().or(m.note.take());
            m.error = None;
        }
        for (outcomes, handler) in [(&mo.steps, false), (&mo.handlers, true)] {
            for so in outcomes {
                let s = step_mut(m, &so.id, &so.kind);
                s.handler = handler;
                s.state = step_state(so.state);
                if s.name.is_none() {
                    s.name = so.name.clone();
                }
                if s.summary == s.id || s.summary.is_empty() {
                    s.summary = so.summary.clone();
                }
                s.result = so.result.clone().or(s.result.take());
                s.started_at = so.started_at.or(s.started_at);
                s.finished_at = so.finished_at.or(s.finished_at);
                s.duration_ms = so.duration_ms.or(s.duration_ms);
                s.error = so.error_detail.as_ref().map(error_view).or(s.error.take());
            }
        }
        // What the events left open (an older agent's outcome has no
        // steps, or events were dropped) never ran once the module ended.
        if matches!(
            mo.status,
            ModuleStatus::Failed
                | ModuleStatus::Unhealthy
                | ModuleStatus::NotStarted
                | ModuleStatus::RolledBack
        ) {
            for s in m.steps.iter_mut().chain(m.handlers.iter_mut()) {
                if matches!(s.state, StepState::Pending | StepState::Running) {
                    s.state = StepState::NotRun;
                }
                if mo.status == ModuleStatus::RolledBack
                    && mo.steps.is_empty()
                    && matches!(s.state, StepState::Changed | StepState::Unchanged)
                {
                    s.state = StepState::RolledBack;
                }
            }
        }
        if let Some(health) = &outcome.health {
            for r in health.results.iter().filter(|r| r.module == mo.module) {
                m.health
                    .push(health_dto(r, &format!("{:?}", health.scope).to_lowercase()));
            }
        }
    }
    for ro in &outcome.removals_run {
        match report
            .removals
            .iter_mut()
            .find(|r| r.resource == ro.resource)
        {
            Some(r) => {
                r.state = step_state(ro.state);
                r.summary = ro.summary.clone();
                r.error = ro.error_detail.as_ref().map(error_view);
            }
            None => report.removals.push(RemovalReport {
                resource: ro.resource.clone(),
                kind: ro.kind.clone(),
                summary: ro.summary.clone(),
                state: step_state(ro.state),
                caution: ro.caution,
                error: ro.error_detail.as_ref().map(error_view),
            }),
        }
    }
    if outcome.removals_run.is_empty() {
        // An older agent: removals it did not report did not run.
        let left = if outcome.succeeded {
            StepState::Skipped
        } else {
            StepState::NotRun
        };
        for r in &mut report.removals {
            if r.state == StepState::Pending {
                r.state = left;
            }
        }
    }
    report.error = outcome.error_detail.as_ref().map(error_view).or_else(|| {
        // An older agent: a run error no module accounts for.
        let modules_explain = outcome.module_results.iter().any(|m| m.error.is_some());
        outcome
            .error
            .as_deref()
            .filter(|_| !modules_explain)
            .map(legacy_error)
    });
    report.started_at = outcome.started_at.or(report.started_at);
    report.finished_at = outcome.finished_at.or(report.finished_at);
}

fn health_dto(r: &buckle_core::health::HealthResult, scope: &str) -> HealthResultDto {
    use buckle_core::health::HealthStatus;
    let (status, reason) = match &r.status {
        HealthStatus::Pass => ("pass", None),
        HealthStatus::Fail { reason } => ("fail", Some(reason.clone())),
        HealthStatus::Unknown { reason } => ("unknown", Some(reason.clone())),
    };
    HealthResultDto {
        scope: scope.to_owned(),
        module: r.module.clone(),
        check: r.check.clone(),
        kind: r.kind.clone(),
        required: r.required,
        status: status.to_owned(),
        reason,
        at: r.at,
        rollout_id: None,
    }
}

/// The report of `run` on its host.
///
/// `plan` is the plan the run follows when the server holds it (a rollout's);
/// `events` are the run's stored apply events; `outcome` is the agent's
/// result once it arrived; `health` are the host's health results recorded
/// during the run and after it until the next one (settle and bake).
pub fn run_report(
    run: &RunRecord,
    plan: Option<&Plan>,
    events: &[RunEvent],
    outcome: Option<&ApplyOutcome>,
    health: &[HealthResultDto],
) -> RunReport {
    let mut report = RunReport {
        id: run.id.clone(),
        host: run.host.clone(),
        revision: run.revision.clone(),
        plan_digest: run.plan_digest.clone(),
        rollout_id: run.rollout.clone(),
        state: run_state(run, !events.is_empty()),
        started_at: (!events.is_empty() || !run.is_running()).then_some(run.started_at),
        finished_at: (!run.is_running()).then_some(run.finished_at),
        updated_at: run.finished_at,
        counts: RunCounts::default(),
        modules: Vec::new(),
        removals: Vec::new(),
        error: None,
        trace_id: run.trace_id.clone(),
        trace_url: None,
        action: run.action.clone(),
    };
    if let Some(plan) = plan {
        from_plan(&mut report, plan);
    }
    let parsed: Vec<(DateTime<Utc>, ApplyEvent)> = events
        .iter()
        .filter_map(|e| match serde_json::from_str::<ApplyEvent>(&e.event) {
            Ok(event) => Some((e.at, event)),
            Err(err) => {
                tracing::debug!(run = %run.id, error = %err, "skipping an apply event this server cannot read");
                None
            }
        })
        .collect();
    let step_ids = parsed
        .iter()
        .any(|(_, e)| e.step().is_some_and(|(_, id)| !id.is_empty()));
    for e in events {
        report.updated_at = report.updated_at.max(e.at);
    }
    for (at, event) in &parsed {
        apply_event(&mut report, event, *at, step_ids);
    }
    if let Some(outcome) = outcome {
        apply_outcome(&mut report, outcome);
    } else if !run.is_running() {
        // Finished without an outcome (an older agent): whatever is still
        // open never ran.
        for m in &mut report.modules {
            for s in m.steps.iter_mut().chain(m.handlers.iter_mut()) {
                if matches!(s.state, StepState::Pending | StepState::Running) {
                    s.state = StepState::NotRun;
                }
            }
            if matches!(m.state, ModuleState::Pending | ModuleState::Running) {
                m.state = ModuleState::NotStarted;
            }
        }
        if let Some(error) = &run.error {
            report.error = Some(legacy_error(error));
        }
    }
    for h in health {
        if let Some(m) = report.modules.iter_mut().find(|m| m.name == h.module)
            && !m
                .health
                .iter()
                .any(|x| x.check == h.check && x.at == h.at && x.scope == h.scope)
        {
            m.health.push(h.clone());
        }
    }
    for m in &mut report.modules {
        m.health.sort_by_key(|h| h.at);
        // Tasks (not handlers) that changed something, as they stand now (a
        // rolled-back step no longer counts); an outcome without per-step
        // data (an older agent) has only its own count.
        let stepless = outcome.and_then(|o| {
            o.module_results
                .iter()
                .find(|mo| mo.module == m.name && mo.steps.is_empty())
        });
        m.changes = match stepless {
            Some(mo) => mo.changes as u32,
            None => m
                .steps
                .iter()
                .filter(|s| s.state == StepState::Changed)
                .count() as u32,
        };
    }
    report.recount();
    report
}

/// The report of a host that has a plan but has not started applying it.
pub fn pending_report(
    host: &str,
    revision: &str,
    rollout: Option<&str>,
    plan: &Plan,
    at: DateTime<Utc>,
) -> RunReport {
    let run = RunRecord {
        id: String::new(),
        host: host.to_owned(),
        rollout: rollout.map(str::to_owned),
        revision: revision.to_owned(),
        plan_digest: plan.digest.to_string(),
        started_at: at,
        finished_at: at,
        outcome: crate::store::RUNNING.to_owned(),
        changes: 0,
        removals: 0,
        error: None,
        trace_id: None,
        outcome_json: None,
        action: None,
    };
    run_report(&run, Some(plan), &[], None, &[])
}

/// The live progress update for one stored event of `run`.
pub fn progress(run: &RunRecord, event: &ApplyEvent, at: DateTime<Utc>) -> RunProgress {
    let mut p = RunProgress {
        host: run.host.clone(),
        run_id: run.id.clone(),
        rollout_id: run.rollout.clone(),
        at,
        run_state: RunState::Running,
        module: None,
        module_state: None,
        step: None,
        step_state: None,
        result: None,
        error: None,
    };
    match event {
        ApplyEvent::ModuleStarted { module } => {
            p.module = Some(module.clone());
            p.module_state = Some(ModuleState::Running);
        }
        ApplyEvent::ModuleFinished { module, status } => {
            p.module = Some(module.clone());
            p.module_state = Some(module_state(*status));
        }
        ApplyEvent::HandlerRun { module, .. } | ApplyEvent::HealthChecked { module, .. } => {
            p.module = Some(module.clone());
        }
        _ => {}
    }
    if let Some((module, id)) = event.step() {
        p.module = Some(module.to_owned());
        p.step = Some(id.to_owned());
        p.step_state = event.step_state().map(step_state);
    }
    match event {
        ApplyEvent::StepFinished { summary, .. } => p.result = Some(summary.clone()),
        ApplyEvent::StepProgress { message, .. } => p.result = Some(message.clone()),
        ApplyEvent::StepBlocked { reason, .. } | ApplyEvent::StepSkipped { reason, .. } => {
            p.result = Some(reason.clone())
        }
        ApplyEvent::StepFailed {
            error,
            error_detail,
            ..
        } => {
            p.error = Some(
                error_detail
                    .as_ref()
                    .map(error_view)
                    .unwrap_or_else(|| legacy_error(error)),
            )
        }
        // A removal: no module, the step is its `resource`.
        ApplyEvent::RemovalRun { resource, summary } => {
            p.step = Some(resource.clone());
            match summary.strip_prefix("failed: ") {
                Some(message) => {
                    p.step_state = Some(StepState::Failed);
                    p.error = Some(legacy_error(message));
                }
                None => {
                    p.step_state = Some(StepState::Changed);
                    p.result = Some(summary.clone());
                }
            }
        }
        _ => {}
    }
    p
}

/// The live progress update for a run that just finished.
pub fn finished_progress(run: &RunRecord, outcome: Option<&ApplyOutcome>) -> RunProgress {
    RunProgress {
        host: run.host.clone(),
        run_id: run.id.clone(),
        rollout_id: run.rollout.clone(),
        at: run.finished_at,
        run_state: run_state(run, true),
        module: None,
        module_state: None,
        step: None,
        step_state: None,
        result: None,
        error: outcome.and_then(outcome_error),
    }
}

/// The key a run's event is stored under: the latest event per step,
/// module, handler, removal or check is kept.
pub fn event_key(event: &ApplyEvent) -> String {
    match event {
        ApplyEvent::ModuleStarted { module } | ApplyEvent::ModuleFinished { module, .. } => {
            format!("module:{module}")
        }
        ApplyEvent::StepStarted {
            module,
            id,
            task_id,
            ..
        }
        | ApplyEvent::StepFinished {
            module,
            id,
            task_id,
            ..
        }
        | ApplyEvent::StepFailed {
            module,
            id,
            task_id,
            ..
        }
        | ApplyEvent::StepBlocked {
            module,
            id,
            task_id,
            ..
        }
        | ApplyEvent::StepSkipped {
            module,
            id,
            task_id,
            ..
        } => format!("step:{module}/{}", if id.is_empty() { task_id } else { id }),
        ApplyEvent::HandlerRun { module, handler } => format!("handler:{module}/{handler}"),
        ApplyEvent::RemovalRun { resource, .. } => format!("removal:{resource}"),
        ApplyEvent::HealthChecked { module, check, .. } => format!("health:{module}/{check}"),
        ApplyEvent::FactsRefreshed { .. } => "facts".to_owned(),
        ApplyEvent::StepProgress {
            module,
            id,
            task_id,
            ..
        } => format!(
            "progress:{module}/{}",
            if id.is_empty() { task_id } else { id }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::engine::{ModuleOutcome, RemovalOutcome, StepOutcome};
    use buckle_core::model::RemovalPolicy;
    use buckle_core::plan::{
        HandlerRun, ModulePlan, Removal, ResourceKey, RevisionId, Step, TaskRef,
    };
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    fn step(module: &str, key: &str) -> Step {
        Step {
            task: TaskRef {
                module: module.into(),
                id: key.into(),
                kind: "file.copy".into(),
                name: Some(format!("copy {key}")),
            },
            resource: ResourceKey::new(module, "file.copy", key),
            planned: Planned::change(Diff::Create {
                summary: format!("create {key}"),
                preview: None,
            }),
            locks: vec![],
            notifies: vec![],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
            facts: None,
        }
    }

    fn plan() -> Plan {
        Plan::new(
            RevisionId::new("r1"),
            "pi-1",
            vec![
                ModulePlan {
                    module: "base".into(),
                    needs: vec![],
                    steps: vec![step("base", "/a"), step("base", "/b"), step("base", "/c")],
                    handlers: vec![HandlerRun {
                        module: "base".into(),
                        handler: "restart".into(),
                        kind: "systemd.restart".into(),
                        summary: "restart base".into(),
                    }],
                    provides_facts: vec![],
                },
                ModulePlan {
                    module: "app".into(),
                    needs: vec!["base".into()],
                    steps: vec![step("app", "/d")],
                    handlers: vec![],
                    provides_facts: vec![],
                },
            ],
            vec![Removal {
                resource: ResourceKey::new("old", "file.copy", "/x"),
                policy: RemovalPolicy::Cleanup,
                summary: "delete /x".into(),
                caution: false,
                introduced_in: None,
            }],
            vec![],
        )
    }

    fn run(outcome: &str) -> RunRecord {
        RunRecord {
            id: "run-1".into(),
            host: "pi-1".into(),
            rollout: Some("ro-1".into()),
            revision: "r1".into(),
            plan_digest: plan().digest.to_string(),
            started_at: t(0),
            finished_at: if outcome == crate::store::RUNNING {
                t(0)
            } else {
                t(60)
            },
            outcome: outcome.into(),
            changes: 0,
            removals: 0,
            error: None,
            trace_id: None,
            outcome_json: None,
            action: None,
        }
    }

    fn ev(seq: i64, event: ApplyEvent) -> RunEvent {
        RunEvent {
            seq,
            event: serde_json::to_string(&event).unwrap(),
            at: t(seq),
        }
    }

    fn started(module: &str, key: &str) -> ApplyEvent {
        ApplyEvent::StepStarted {
            module: module.into(),
            task_id: key.into(),
            id: format!("file.copy[{key}]"),
            kind: "file.copy".into(),
        }
    }

    fn finished(module: &str, key: &str) -> ApplyEvent {
        ApplyEvent::StepFinished {
            module: module.into(),
            task_id: key.into(),
            id: format!("file.copy[{key}]"),
            kind: "file.copy".into(),
            changed: true,
            summary: format!("wrote {key}"),
        }
    }

    fn states(report: &RunReport, module: &str) -> Vec<(String, StepState)> {
        report
            .modules
            .iter()
            .find(|m| m.name == module)
            .unwrap()
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.state))
            .collect()
    }

    fn module<'a>(report: &'a RunReport, name: &str) -> &'a ModuleReport {
        report.modules.iter().find(|m| m.name == name).unwrap()
    }

    #[test]
    fn before_any_event_everything_planned_is_pending() {
        let report = pending_report("pi-1", "r1", Some("ro-1"), &plan(), t(0));
        assert_eq!(report.state, RunState::Pending);
        assert_eq!(report.id, "", "a run that has not started has no id yet");
        assert!(
            report
                .modules
                .iter()
                .all(|m| m.state == ModuleState::Pending)
        );
        assert_eq!(
            report.counts.steps.pending,
            4 + 1 + 1,
            "steps, a handler and a removal"
        );
        let first = &module(&report, "base").steps[0];
        assert_eq!(first.id, "file.copy[/a]");
        assert_eq!(first.summary, "create /a");
        assert_eq!(first.name.as_deref(), Some("copy /a"));
        assert_eq!(first.diff, Some(DiffKind::Create));
        assert_eq!(module(&report, "base").handlers[0].id, "base:restart");
        assert!(module(&report, "base").handlers[0].handler);
        assert_eq!(report.removals[0].state, StepState::Pending);
    }

    #[test]
    fn a_running_step_shows_its_progress_until_it_finishes() {
        let progress = |message: &str| ApplyEvent::StepProgress {
            module: "base".into(),
            task_id: "/b".into(),
            id: "file.copy[/b]".into(),
            kind: "file.copy".into(),
            message: message.into(),
        };
        let result = |events: Vec<RunEvent>| {
            let report = run_report(
                &run(crate::store::RUNNING),
                Some(&plan()),
                &events,
                None,
                &[],
            );
            let s = &module(&report, "base").steps[1];
            (s.state, s.result.clone())
        };
        let running = vec![
            ev(1, started("base", "/b")),
            ev(2, progress("deploying: web 1/2 healthy")),
        ];
        assert_eq!(
            result(running.clone()),
            (
                StepState::Running,
                Some("deploying: web 1/2 healthy".into())
            )
        );
        let mut done = running;
        done.push(ev(3, finished("base", "/b")));
        let (state, text) = result(done);
        assert_eq!(state, StepState::Changed);
        assert_ne!(text.as_deref(), Some("deploying: web 1/2 healthy"));
        assert_ne!(event_key(&progress("x")), event_key(&started("base", "/b")));
    }

    #[test]
    fn while_applying_the_started_step_and_its_module_are_running() {
        let events = vec![
            ev(
                1,
                ApplyEvent::ModuleStarted {
                    module: "base".into(),
                },
            ),
            ev(2, finished("base", "/a")),
            ev(3, started("base", "/b")),
        ];
        let report = run_report(
            &run(crate::store::RUNNING),
            Some(&plan()),
            &events,
            None,
            &[],
        );
        assert_eq!(report.state, RunState::Running);
        assert_eq!(module(&report, "base").state, ModuleState::Running);
        assert_eq!(module(&report, "app").state, ModuleState::Pending);
        assert_eq!(
            states(&report, "base"),
            vec![
                ("file.copy[/a]".to_string(), StepState::Changed),
                ("file.copy[/b]".to_string(), StepState::Running),
                ("file.copy[/c]".to_string(), StepState::Pending),
            ]
        );
        let a = &module(&report, "base").steps[0];
        assert_eq!(a.result.as_deref(), Some("wrote /a"));
        assert_eq!(module(&report, "base").changes, 1);
        assert_eq!(report.updated_at, t(3));
        assert_eq!(report.counts.steps.running, 1);
    }

    #[test]
    fn a_failed_step_event_carries_its_structured_error_and_ends_the_module() {
        let detail = ErrorReport::user("mkswap failed")
            .with_causes(["exit status 1"])
            .with_advice(["Check the swap file size."]);
        let events = vec![
            ev(
                1,
                ApplyEvent::ModuleStarted {
                    module: "base".into(),
                },
            ),
            ev(2, finished("base", "/a")),
            ev(
                3,
                ApplyEvent::StepFailed {
                    module: "base".into(),
                    task_id: "/b".into(),
                    id: "file.copy[/b]".into(),
                    kind: "file.copy".into(),
                    error: "mkswap failed".into(),
                    error_detail: Some(detail),
                },
            ),
            ev(
                4,
                ApplyEvent::ModuleFinished {
                    module: "base".into(),
                    status: ModuleStatus::Failed,
                },
            ),
        ];
        let report = run_report(
            &run(crate::store::RUNNING),
            Some(&plan()),
            &events,
            None,
            &[],
        );
        let base = module(&report, "base");
        assert_eq!(base.state, ModuleState::Failed);
        assert_eq!(base.steps[2].state, StepState::NotRun);
        let error = base.steps[1].error.as_ref().unwrap();
        assert_eq!(error.message, "mkswap failed");
        assert_eq!(error.causes, vec!["exit status 1"]);
        assert_eq!(error.advice, vec!["Check the swap file size."]);
    }

    fn outcome(status: ModuleStatus, steps: Vec<(&str, engine::StepState)>) -> ApplyOutcome {
        let step_outcome = |id: &str, state| StepOutcome {
            id: id.into(),
            kind: "file.copy".into(),
            name: None,
            summary: "s".into(),
            state,
            result: None,
            started_at: Some(t(1)),
            finished_at: Some(t(2)),
            duration_ms: Some(1000),
            error_detail: (state == engine::StepState::Failed)
                .then(|| ErrorReport::user("boom").with_advice(["fix it"])),
        };
        ApplyOutcome {
            plan_digest: plan().digest,
            succeeded: status == ModuleStatus::Applied,
            changes: 1,
            removals: 0,
            duration_ms: 60_000,
            module_results: vec![
                ModuleOutcome {
                    steps: steps.iter().map(|(id, s)| step_outcome(id, *s)).collect(),
                    error_detail: (status == ModuleStatus::Failed)
                        .then(|| ErrorReport::user("base failed: boom")),
                    error: (status == ModuleStatus::Failed).then(|| "base failed: boom".into()),
                    ..ModuleOutcome::new("base", status)
                },
                ModuleOutcome {
                    steps: vec![step_outcome("file.copy[/d]", engine::StepState::NotRun)],
                    ..ModuleOutcome::new("app", ModuleStatus::NotStarted)
                },
            ],
            failed_module: None,
            rolled_back_modules: vec![],
            not_started: vec!["app".into()],
            health: None,
            error: None,
            error_detail: None,
            removals_run: vec![RemovalOutcome {
                resource: "old/file.copy[/x]".into(),
                kind: "file.copy".into(),
                summary: "delete /x".into(),
                state: engine::StepState::NotRun,
                caution: false,
                error_detail: None,
            }],
            started_at: Some(t(0)),
            finished_at: Some(t(60)),
            reboot: None,
        }
    }

    #[rstest]
    #[case::failed(
        "failed",
        ModuleStatus::Failed,
        vec![("file.copy[/a]", engine::StepState::Changed), ("file.copy[/b]", engine::StepState::Failed), ("file.copy[/c]", engine::StepState::NotRun)],
        RunState::Failed,
        vec![StepState::Changed, StepState::Failed, StepState::NotRun]
    )]
    #[case::rolled_back(
        "failed",
        ModuleStatus::RolledBack,
        vec![("file.copy[/a]", engine::StepState::RolledBack), ("file.copy[/b]", engine::StepState::Failed), ("file.copy[/c]", engine::StepState::NotRun)],
        RunState::Failed,
        vec![StepState::RolledBack, StepState::Failed, StepState::NotRun]
    )]
    #[case::succeeded(
        "succeeded",
        ModuleStatus::Applied,
        vec![("file.copy[/a]", engine::StepState::Changed), ("file.copy[/b]", engine::StepState::Unchanged), ("file.copy[/c]", engine::StepState::Changed)],
        RunState::Succeeded,
        vec![StepState::Changed, StepState::Unchanged, StepState::Changed]
    )]
    fn once_the_result_arrives_states_come_from_the_outcome(
        #[case] stored: &str,
        #[case] status: ModuleStatus,
        #[case] steps: Vec<(&str, engine::StepState)>,
        #[case] run_state: RunState,
        #[case] expected: Vec<StepState>,
    ) {
        // Events said /b was still running; the outcome knows better.
        let events = vec![ev(1, finished("base", "/a")), ev(2, started("base", "/b"))];
        let outcome = outcome(status, steps);
        let report = run_report(&run(stored), Some(&plan()), &events, Some(&outcome), &[]);
        assert_eq!(report.state, run_state);
        assert_eq!(
            states(&report, "base")
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(module(&report, "base").state, module_state(status));
        assert_eq!(module(&report, "app").state, ModuleState::NotStarted);
        assert_eq!(states(&report, "app")[0].1, StepState::NotRun);
        assert_eq!(report.removals[0].state, StepState::NotRun);
        assert_eq!(report.finished_at, Some(t(60)));
        assert_eq!(report.counts.steps.running, 0);
        assert_eq!(
            module(&report, "base").changes,
            expected
                .iter()
                .filter(|s| **s == StepState::Changed)
                .count() as u32,
            "changes count the tasks that stand changed"
        );
        if status == ModuleStatus::Failed {
            let failed = &module(&report, "base").steps[1];
            assert_eq!(failed.error.as_ref().unwrap().advice, vec!["fix it"]);
            assert_eq!(
                module(&report, "base").error.as_ref().unwrap().message,
                "base failed: boom"
            );
        }
    }

    #[test]
    fn without_a_plan_the_report_is_built_from_events_and_outcome() {
        let events = vec![
            ev(
                1,
                ApplyEvent::ModuleStarted {
                    module: "base".into(),
                },
            ),
            ev(2, started("base", "/a")),
        ];
        let report = run_report(&run(crate::store::RUNNING), None, &events, None, &[]);
        assert_eq!(
            states(&report, "base"),
            vec![("file.copy[/a]".to_string(), StepState::Running)]
        );
    }

    #[test]
    fn an_older_agents_outcome_settles_open_steps_and_keeps_its_error_line() {
        let events = vec![ev(1, finished("base", "/a")), ev(2, started("base", "/b"))];
        let mut outcome = outcome(ModuleStatus::Failed, vec![]);
        outcome.removals_run.clear();
        for m in &mut outcome.module_results {
            m.steps.clear();
            m.error_detail = None;
        }
        let report = run_report(&run("failed"), Some(&plan()), &events, Some(&outcome), &[]);
        assert_eq!(
            states(&report, "base")
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            vec![StepState::Changed, StepState::NotRun, StepState::NotRun]
        );
        assert_eq!(
            module(&report, "base").error,
            Some(ErrorView::system("base failed: boom"))
        );
        assert_eq!(report.removals[0].state, StepState::NotRun);
    }

    #[test]
    fn an_outcome_without_steps_keeps_its_change_count() {
        let mut outcome = outcome(ModuleStatus::Applied, vec![]);
        outcome.module_results[0].changes = 2;
        let report = run_report(&run("succeeded"), Some(&plan()), &[], Some(&outcome), &[]);
        assert_eq!(module(&report, "base").changes, 2);
        let with_steps = outcome_with_steps();
        let report = run_report(
            &run("succeeded"),
            Some(&plan()),
            &[],
            Some(&with_steps),
            &[],
        );
        assert_eq!(module(&report, "base").changes, 1, "counted from the steps");
    }

    fn outcome_with_steps() -> ApplyOutcome {
        let mut o = outcome(
            ModuleStatus::Applied,
            vec![
                ("file.copy[/a]", engine::StepState::Changed),
                ("file.copy[/b]", engine::StepState::Unchanged),
                ("file.copy[/c]", engine::StepState::Unchanged),
            ],
        );
        o.module_results[0].changes = 7;
        o
    }

    #[rstest]
    #[case::newer_agent(true, StepState::Unchanged)]
    #[case::older_agent(false, StepState::Changed)]
    fn a_handler_run_on_a_pending_handler_depends_on_the_agent(
        #[case] step_ids: bool,
        #[case] expected: StepState,
    ) {
        let mut first = finished("base", "/a");
        if !step_ids && let ApplyEvent::StepFinished { id, .. } = &mut first {
            id.clear();
        }
        let events = vec![
            ev(1, first),
            ev(
                2,
                ApplyEvent::HandlerRun {
                    module: "base".into(),
                    handler: "restart".into(),
                },
            ),
        ];
        let report = run_report(
            &run(crate::store::RUNNING),
            Some(&plan()),
            &events,
            None,
            &[],
        );
        assert_eq!(module(&report, "base").handlers[0].state, expected);
    }

    #[test]
    fn a_handler_run_after_its_own_step_events_keeps_their_state() {
        let started = ApplyEvent::StepStarted {
            module: "base".into(),
            task_id: "base:restart".into(),
            id: "base:restart".into(),
            kind: "systemd.restart".into(),
        };
        let events = vec![
            ev(1, started),
            ev(
                2,
                ApplyEvent::HandlerRun {
                    module: "base".into(),
                    handler: "restart".into(),
                },
            ),
        ];
        let report = run_report(
            &run(crate::store::RUNNING),
            Some(&plan()),
            &events,
            None,
            &[],
        );
        assert_eq!(
            module(&report, "base").handlers[0].state,
            StepState::Changed
        );
    }

    #[test]
    fn health_results_attach_to_their_module() {
        let dto = HealthResultDto {
            scope: "bake".into(),
            module: "base".into(),
            check: "api".into(),
            kind: "http".into(),
            required: true,
            status: "pass".into(),
            reason: None,
            at: t(70),
            rollout_id: None,
        };
        let report = run_report(
            &run("succeeded"),
            Some(&plan()),
            &[],
            None,
            &[dto.clone(), dto],
        );
        assert_eq!(module(&report, "base").health.len(), 1, "deduplicated");
        assert!(module(&report, "app").health.is_empty());
    }

    #[test]
    fn progress_describes_the_step_and_its_state() {
        let p = progress(&run(crate::store::RUNNING), &started("base", "/b"), t(5));
        assert_eq!(p.run_id, "run-1");
        assert_eq!(p.rollout_id.as_deref(), Some("ro-1"));
        assert_eq!(p.module.as_deref(), Some("base"));
        assert_eq!(p.step.as_deref(), Some("file.copy[/b]"));
        assert_eq!(p.step_state, Some(StepState::Running));
        assert_eq!(p.run_state, RunState::Running);
        let removal = progress(
            &run(crate::store::RUNNING),
            &ApplyEvent::RemovalRun {
                resource: "old/file.copy[/x]".into(),
                summary: "deleted /x".into(),
            },
            t(6),
        );
        assert_eq!(removal.module, None);
        assert_eq!(removal.step.as_deref(), Some("old/file.copy[/x]"));
        assert_eq!(removal.step_state, Some(StepState::Changed));
        let done = finished_progress(&run("failed"), Some(&outcome(ModuleStatus::Failed, vec![])));
        assert_eq!(done.run_state, RunState::Failed);
        assert_eq!(
            done.error.map(|e| e.message).as_deref(),
            Some("base failed: boom")
        );
    }
}
