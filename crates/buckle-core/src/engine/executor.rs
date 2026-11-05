//! The mutating half of the engine: drives the host from a fresh plan.
//!
//! Modules run concurrently along the `needs` DAG (bounded by
//! `Inputs::parallelism`); tasks within a module run in order; steps holding
//! resource locks serialise across modules; handlers run after a module's
//! steps; the module's health checks run with settle semantics; a module with
//! `facts.provides` triggers a facts refresh and the planning of deferred
//! dependents; a failing module is rolled back from the stable revision and
//! the run halts; removals run last, only when nothing halted.

use super::checks;
use super::planner::{
    Contexts, DeferredPlan, Planner, RichHandler, RichModulePlan, RichPlan, RichStep,
    collect_facts, plan_with,
};
use super::{
    ApplyEvent, ApplyOutcome, Inputs, ModuleOutcome, ModuleStatus, RemovalOutcome, StepOutcome,
    StepState,
};
use crate::Digest;
use crate::error::ErrorReport;
use crate::facts::Facts;
use crate::health::{HealthReport, HealthResult, HealthScope};
use crate::model::{RemovalPolicy, RetryPolicy};
use crate::plan::{HandlerRun, Plan, Planned, RevisionId, Step};
use crate::secrets::redact::Redactor;
use crate::state::{ResourceRecord, RunOutcome, RunRecord, State};
use crate::task::{Applied, Cx, DynTask, ErasedSpec, Mode};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;
use tracing::Instrument;

/// Named resource locks, created on first use.
#[derive(Default)]
struct LockTable {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl LockTable {
    /// Acquires every named lock in sorted order (so two steps holding
    /// overlapping lock sets cannot deadlock) and returns the guards.
    async fn acquire(&self, names: &[String]) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut sorted: Vec<&String> = names.iter().collect();
        sorted.sort();
        sorted.dedup();
        let mut guards = Vec::with_capacity(sorted.len());
        for name in sorted {
            let lock = {
                let mut table = self.locks.lock().unwrap_or_else(|p| p.into_inner());
                table
                    .entry(name.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone()
            };
            guards.push(lock.lock_owned().await);
        }
        guards
    }
}

/// How a module run ended.
struct ModuleRun {
    outcome: ModuleOutcome,
    health: Vec<HealthResult>,
    /// Set when a task, handler or health gate failed.
    failed: bool,
    /// Steps still blocked at the end of the module (resource and reason).
    blocked: Vec<String>,
}

type ModuleFuture<'a> = Pin<Box<dyn Future<Output = ModuleRun> + Send + 'a>>;

struct Executor<'a> {
    inputs: &'a Inputs,
    events: Sender<ApplyEvent>,
    locks: LockTable,
    state: Arc<Mutex<State>>,
    redactor: Redactor,
    /// Steps that changed something so far in this run; gates re-planning of blocked steps.
    changed_so_far: AtomicUsize,
    /// The facts as refreshed so far in this run.
    facts: Mutex<Facts>,
    /// Modules of this run that have not started yet.
    scheduled: Mutex<HashSet<String>>,
    /// State saves that failed during the run (reported, never fatal: the
    /// host was changed either way).
    save_errors: Mutex<Vec<ErrorReport>>,
    /// Cross-module handlers waiting for their owner module to finish:
    /// owner → `(owner, handler)` pairs.
    pending_handlers: Mutex<HashMap<String, Vec<(String, String)>>>,
    /// The reboot the run's steps and handlers asked for so far.
    reboot: Mutex<Option<crate::reboot::RebootRequest>>,
    /// Whether steps are recorded in state. An action's run owns nothing,
    /// so it records nothing and leaves the state file alone.
    record: bool,
}

/// `Engine::apply`.
pub(super) async fn apply(
    inputs: &Inputs,
    plan: &Plan,
    events: Sender<ApplyEvent>,
) -> crate::Result<ApplyOutcome> {
    let started = tokio::time::Instant::now();
    let started_at = inputs.system.now();
    let planner = Planner::from_inputs(inputs, &inputs.revision)?;
    let facts = collect_facts(inputs, &planner).await;
    let state = inputs.state.load()?;
    let rich = plan_with(&planner, &facts, &state).await?;
    // The handed plan binds what the apply does (its intent), not the text
    // it renders: a re-plan that only differs in content (a fact, a clock, a
    // fingerprint) runs from the fresh plan.
    if rich.plan.intent_digest() != plan.intent_digest() {
        return Err(crate::error::wrap_user(
            super::StalePlan {
                planned: plan.digest.clone(),
                fresh: rich.plan.digest.clone(),
            },
            format!(
                "The plan is stale: it was computed as {} but this host now plans as {}.",
                plan.digest.short(),
                rich.plan.digest.short()
            ),
            &[
                "The host or the configuration changed since the plan was made. Run `buckle plan` again and review the new plan before applying.",
            ],
        ));
    }

    let executor = Executor {
        inputs,
        events,
        locks: LockTable::default(),
        state: Arc::new(Mutex::new(state)),
        redactor: Redactor::new(&inputs.secrets),
        changed_so_far: AtomicUsize::new(0),
        facts: Mutex::new(facts),
        scheduled: Mutex::new(rich.order.iter().cloned().collect()),
        pending_handlers: Mutex::new(HashMap::new()),
        save_errors: Mutex::new(Vec::new()),
        reboot: Mutex::new(None),
        record: true,
    };
    let outcome = executor.run(&planner, rich, started, started_at).await?;
    Ok(outcome)
}

/// `Engine::run_action`: re-plans the action, refuses when its intent no
/// longer matches `plan`, then runs its step, the handlers it notifies and
/// (when it changed something) the module's health checks with settle. It
/// records nothing in state and rolls nothing back.
pub(super) async fn apply_action(
    inputs: &Inputs,
    module: &str,
    action: &str,
    plan: &Plan,
    events: Sender<ApplyEvent>,
) -> crate::Result<ApplyOutcome> {
    let started = tokio::time::Instant::now();
    let started_at = inputs.system.now();
    let planner = Planner::from_inputs(inputs, &inputs.revision)?;
    let facts = collect_facts(inputs, &planner).await;
    let state = inputs.state.load()?;
    let (rich, warnings) =
        super::planner::plan_action_with(&planner, &facts, &state, module, action).await?;
    let fresh = super::planner::action_plan(&planner, &rich, warnings);
    if fresh.intent_digest() != plan.intent_digest() {
        return Err(crate::error::wrap_user(
            super::StalePlan {
                planned: plan.digest.clone(),
                fresh: fresh.digest.clone(),
            },
            format!(
                "The action's plan is stale: it was computed as {} but the host now plans it as {}.",
                plan.digest.short(),
                fresh.digest.short()
            ),
            &["The host changed since the action was planned; plan it again and review it."],
        ));
    }
    let executor = Executor {
        inputs,
        events,
        locks: LockTable::default(),
        state: Arc::new(Mutex::new(state)),
        redactor: Redactor::new(&inputs.secrets),
        changed_so_far: AtomicUsize::new(0),
        facts: Mutex::new(facts),
        scheduled: Mutex::new(HashSet::new()),
        pending_handlers: Mutex::new(HashMap::new()),
        save_errors: Mutex::new(Vec::new()),
        reboot: Mutex::new(None),
        record: false,
    };
    let contexts = planner.contexts(&executor.facts());
    let revision = planner.revision.id.clone();
    let span = tracing::info_span!("action.run", buckle.module = %module, buckle.action = %action);
    let run = executor
        .run_module(&planner, Arc::new(rich), &contexts, &revision, true)
        .instrument(span)
        .await;
    let succeeded = !run.failed && run.blocked.is_empty();
    let error = (!succeeded)
        .then(|| {
            run.outcome
                .error
                .clone()
                .map(|e| format!("module '{module}': {e}"))
        })
        .flatten();
    let failed_module = run.failed.then(|| module.to_owned());
    let outcome = ApplyOutcome {
        plan_digest: fresh.digest.clone(),
        succeeded,
        changes: run.outcome.changes,
        removals: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        module_results: vec![run.outcome],
        failed_module,
        rolled_back_modules: Vec::new(),
        not_started: Vec::new(),
        health: Some(HealthReport {
            scope: HealthScope::Bake,
            results: run.health,
        }),
        error,
        error_detail: None,
        removals_run: Vec::new(),
        started_at: Some(started_at),
        finished_at: Some(inputs.system.now()),
        reboot: executor
            .reboot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take(),
    };
    tracing::info!(succeeded, changes = outcome.changes, "action finished");
    Ok(outcome)
}

impl<'a> Executor<'a> {
    async fn emit(&self, event: ApplyEvent) {
        let _ = self.events.send(event).await;
    }

    /// `err` as a structured report, every field redacted.
    fn report(&self, err: &crate::Error) -> ErrorReport {
        ErrorReport::of(err).map_text(|t| self.scrub(t))
    }

    fn scrub(&self, text: &str) -> String {
        if self.redactor.is_empty() {
            text.to_owned()
        } else {
            self.redactor.redact(text).into_inner()
        }
    }

    /// A snapshot of the facts as refreshed so far.
    fn facts(&self) -> Facts {
        self.facts.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Force-collects `providers` and merges their results into the run's
    /// facts (unless the facts are frozen), then reports the refresh.
    async fn refresh_facts(&self, planner: &Planner<'_>, providers: &[String]) {
        if !self.inputs.facts_frozen {
            let mut fresh = self.facts();
            crate::facts::collect(&planner.facts_cx(), &mut fresh, Some(providers), true).await;
            self.facts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .merge_providers(&fresh, providers);
        }
        self.emit(ApplyEvent::FactsRefreshed {
            providers: providers.to_vec(),
        })
        .await;
    }

    /// Folds a step's or handler's reboot request into the run's.
    fn request_reboot(&self, applied: &Applied, by: String) {
        let Some(request) = applied.reboot.clone() else {
            return;
        };
        tracing::info!(by = %by, delay = ?request.delay, "a reboot was requested; the host reboots after the run");
        let mut request = request;
        request.requested_by = vec![by];
        let mut slot = self.reboot.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_mut() {
            Some(existing) => existing.merge(request),
            None => *slot = Some(request),
        }
    }

    /// Whether anything in the run has asked for a reboot yet.
    fn reboot_pending(&self) -> bool {
        self.reboot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut state)
    }

    fn save_state(&self) -> crate::Result<()> {
        let snapshot = self.with_state(|s| s.clone());
        self.inputs.state.save(&snapshot)
    }

    /// Saves state now (after every step, so a crash loses at most the step
    /// in flight); a failure is remembered for the outcome, not fatal.
    fn checkpoint(&self) {
        if !self.record {
            return;
        }
        if let Err(e) = self.save_state() {
            let report = self.report(&e).prefixed("state could not be saved");
            let mut errors = self.save_errors.lock().unwrap_or_else(|p| p.into_inner());
            if errors.last() != Some(&report) {
                errors.push(report);
            }
        }
    }

    /// Applies a task (or handler) with the declaration's retry policy.
    async fn apply_with_retry(
        &self,
        task: &'static dyn DynTask,
        cx: &Cx,
        spec: &ErasedSpec,
        planned: &Planned,
        retry: &RetryPolicy,
        label: &str,
    ) -> crate::Result<Applied> {
        let mut attempt = 0u32;
        loop {
            match task.apply(cx, spec, planned).await {
                Ok(applied) => return Ok(applied),
                Err(err) if attempt < retry.limit => {
                    attempt += 1;
                    tracing::warn!(
                        error = %err.description(),
                        attempt,
                        limit = retry.limit,
                        "{label} failed; retrying in {:?}",
                        retry.delay
                    );
                    tokio::time::sleep(retry.delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Refreshes (or creates) the record of a resource after a step, keeping
    /// whatever the task did not report from the existing record.
    fn record_step(
        &self,
        step: &Step,
        rs: &RichStep,
        applied: Option<&Applied>,
        revision: &RevisionId,
    ) {
        if !self.record {
            return;
        }
        let now = self.inputs.system.now();
        self.with_state(|state| {
            let notifies: Vec<String> = step
                .notifies
                .iter()
                .map(|n| {
                    let (m, h) = super::planner::qualify_handler(&step.task.module, n);
                    format!("{m}:{h}")
                })
                .collect();
            if notifies.is_empty() {
                state.notifies.remove(&step.resource.to_string());
            } else {
                state.notifies.insert(step.resource.to_string(), notifies);
            }
            let existing = state.get(&step.resource).cloned();
            state.record(ResourceRecord {
                key: step.resource.clone(),
                spec_digest: Digest::of_json(&rs.spec.json),
                shadow: applied
                    .and_then(|a| a.shadow.clone())
                    .or_else(|| existing.as_ref().and_then(|e| e.shadow.clone())),
                content_digest: applied
                    .and_then(|a| a.content_digest.clone())
                    .or_else(|| existing.as_ref().and_then(|e| e.content_digest.clone())),
                // An apply that changed nothing, or reported no artefacts,
                // does not wipe what the resource is known to own.
                artefacts: match (applied, existing.as_ref()) {
                    (Some(a), _) if a.changed && !a.artefacts.is_empty() => a.artefacts.clone(),
                    (_, Some(e)) if !e.artefacts.is_empty() => e.artefacts.clone(),
                    (Some(a), _) => a.artefacts.clone(),
                    (None, _) => Vec::new(),
                },
                removal: step.removal,
                revision: revision.clone(),
                applied_at: now,
                introduced_in: existing
                    .map(|e| e.introduced_in)
                    .unwrap_or_else(|| revision.clone()),
            });
        });
    }

    /// Applies one changing step under its locks, with retries, and records the
    /// resource. `Err` carries the scrubbed, structured error.
    async fn apply_step(
        &self,
        module: &str,
        step: &Step,
        rs: &RichStep,
        planned: &Planned,
        revision: &RevisionId,
        span: &tracing::Span,
    ) -> Result<Applied, ErrorReport> {
        let id = super::step_id(&step.resource);
        self.emit(ApplyEvent::StepStarted {
            module: module.to_owned(),
            task_id: step.task.id.clone(),
            id: id.clone(),
            kind: step.task.kind.clone(),
        })
        .await;
        let mut cx = rs.cx.clone();
        cx.mode = Mode::Apply;
        cx.system = self.inputs.system.clone();
        cx.previous = self
            .with_state(|s| s.get(&step.resource).cloned())
            .map(Arc::new);
        let result = async {
            let _guards = self.locks.acquire(&step.locks).await;
            self.apply_with_retry(
                rs.task,
                &cx,
                &rs.spec,
                planned,
                &rs.retry,
                &format!("{}", step.resource),
            )
            .await
        }
        .instrument(span.clone())
        .await;
        match result {
            Ok(applied) => {
                span.record("buckle.changed", applied.changed);
                if applied.changed {
                    self.changed_so_far.fetch_add(1, Ordering::Relaxed);
                }
                self.request_reboot(&applied, format!("{module}/{id}"));
                self.record_step(step, rs, Some(&applied), revision);
                self.checkpoint();
                self.emit(ApplyEvent::StepFinished {
                    module: module.to_owned(),
                    task_id: step.task.id.clone(),
                    id,
                    kind: step.task.kind.clone(),
                    changed: applied.changed,
                    summary: self.scrub(&applied.summary),
                })
                .await;
                Ok(applied)
            }
            Err(err) => {
                let report = self.report(&err);
                tracing::error!(parent: span, error = %report.message, causes = ?report.causes, "task failed");
                self.emit(ApplyEvent::StepFailed {
                    module: module.to_owned(),
                    task_id: step.task.id.clone(),
                    id,
                    kind: step.task.kind.clone(),
                    error: report.one_line(),
                    error_detail: Some(report.clone()),
                })
                .await;
                Err(report)
            }
        }
    }

    /// Whether `owner` is a module of this run that has not started and needs
    /// `notifier` (so it certainly runs after it).
    fn runs_later(&self, planner: &Planner<'_>, owner: &str, notifier: &str) -> bool {
        self.scheduled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(owner)
            && super::planner::ancestors(planner.revision, owner).contains(notifier)
    }

    fn take_pending(&self, owner: &str) -> Vec<(String, String)> {
        self.pending_handlers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(owner)
            .unwrap_or_default()
    }

    /// Plans (deduplicated and ordered) and runs the notified handlers. Returns
    /// what happened to each handler and, when one failed (or they could not
    /// be planned), the error.
    async fn run_notified(
        &self,
        planner: &Planner<'_>,
        module: &str,
        ctx: Option<&Arc<crate::selectors::HostContext>>,
        notified: &[(String, String)],
        contexts: &Contexts,
    ) -> (Vec<StepOutcome>, Result<(), ErrorReport>) {
        if notified.is_empty() {
            return (Vec::new(), Ok(()));
        }
        let (runs, handlers) = match planner.plan_notified(module, ctx, notified, contexts).await {
            Ok(planned) => planned,
            Err(e) => {
                return (
                    Vec::new(),
                    Err(self.report(&e).prefixed("planning handlers failed")),
                );
            }
        };
        let mut outcomes = Vec::with_capacity(runs.len());
        for (run, rh) in runs.iter().zip(handlers.iter()) {
            let outcome = self.run_handler(module, run, rh).await;
            let failed = outcome.state == StepState::Failed;
            let error = outcome.error_detail.clone();
            outcomes.push(outcome);
            if failed {
                // The handlers after the failing one never run.
                for later in runs.iter().skip(outcomes.len()) {
                    outcomes.push(StepOutcome::planned_handler(later, StepState::NotRun));
                }
                let error = error
                    .unwrap_or_else(|| ErrorReport::system("the handler failed"))
                    .prefixed(&format!("handler {}:{} failed", run.module, run.handler));
                return (outcomes, Err(error));
            }
        }
        (outcomes, Ok(()))
    }

    /// Runs one handler (when its plan is a change) and emits its events.
    async fn run_handler(&self, module: &str, run: &HandlerRun, rh: &RichHandler) -> StepOutcome {
        let span = tracing::info_span!(
            "handler.run",
            buckle.module = %run.module,
            buckle.handler = %run.handler,
            buckle.task.kind = %run.kind,
        );
        let id = super::handler_id(&run.module, &run.handler);
        let mut outcome = StepOutcome::planned_handler(run, StepState::Unchanged);
        let started_at = self.inputs.system.now();
        let clock = tokio::time::Instant::now();
        outcome.started_at = Some(started_at);
        if rh.planned.is_change() {
            self.emit(ApplyEvent::StepStarted {
                module: module.to_owned(),
                task_id: id.clone(),
                id: id.clone(),
                kind: run.kind.clone(),
            })
            .await;
            let mut cx = rh.cx.clone();
            cx.mode = Mode::Apply;
            cx.system = self.inputs.system.clone();
            let locks: Vec<String> = rh.task.locks_for(&rh.spec);
            let result = async {
                let _guards = self.locks.acquire(&locks).await;
                self.apply_with_retry(
                    rh.task,
                    &cx,
                    &rh.spec,
                    &rh.planned,
                    &rh.retry,
                    &format!("handler {}:{}", run.module, run.handler),
                )
                .await
            }
            .instrument(span.clone())
            .await;
            outcome.finished_at = Some(self.inputs.system.now());
            outcome.duration_ms = Some(clock.elapsed().as_millis() as u64);
            match result {
                Err(err) => {
                    let report = self.report(&err);
                    tracing::error!(parent: &span, error = %report.message, causes = ?report.causes, "handler failed");
                    self.emit(ApplyEvent::StepFailed {
                        module: module.to_owned(),
                        task_id: id.clone(),
                        id,
                        kind: run.kind.clone(),
                        error: report.one_line(),
                        error_detail: Some(report.clone()),
                    })
                    .await;
                    outcome.state = StepState::Failed;
                    outcome.error_detail = Some(report);
                    return outcome;
                }
                Ok(applied) => {
                    self.request_reboot(&applied, id.clone());
                    let summary = self.scrub(&applied.summary);
                    outcome.state = if applied.changed {
                        StepState::Changed
                    } else {
                        StepState::Unchanged
                    };
                    self.emit(ApplyEvent::StepFinished {
                        module: module.to_owned(),
                        task_id: id.clone(),
                        id: id.clone(),
                        kind: run.kind.clone(),
                        changed: applied.changed,
                        summary: summary.clone(),
                    })
                    .await;
                    outcome.result = Some(summary);
                }
            }
        } else {
            outcome.finished_at = Some(started_at);
            outcome.duration_ms = Some(0);
            outcome.result = Some("no change".to_owned());
        }
        self.emit(ApplyEvent::HandlerRun {
            module: run.module.clone(),
            handler: run.handler.clone(),
        })
        .await;
        outcome
    }

    /// Runs one module: steps in order, then handlers, then (optionally) the
    /// module's health checks with settle.
    ///
    /// A step that was `Blocked` at plan time is planned again once anything
    /// earlier in the run changed, because an earlier step may have installed
    /// the tool it was missing. If it now plans a change that deletes nothing,
    /// the change is applied and the handlers it notifies run alongside the
    /// planned ones; a deletion was never reviewed, so it stays blocked. A
    /// module that ends with blocked steps is `Blocked`: the run needs
    /// attention and does not count as a success.
    async fn run_module(
        &self,
        planner: &Planner<'_>,
        rich: Arc<RichModulePlan>,
        contexts: &Contexts,
        revision: &RevisionId,
        run_health: bool,
    ) -> ModuleRun {
        let module = rich.plan.module.clone();
        let started = tokio::time::Instant::now();
        let started_at = self.inputs.system.now();
        self.emit(ApplyEvent::ModuleStarted {
            module: module.clone(),
        })
        .await;
        let mut changes = 0usize;
        let mut health = Vec::new();
        // Steps whose apply really changed something: only they notify.
        let mut changed_steps: Vec<Step> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        // Every planned step, `not_run` until it is reached.
        let mut steps: Vec<StepOutcome> = rich
            .plan
            .steps
            .iter()
            .map(|s| StepOutcome::planned(s, StepState::NotRun))
            .collect();
        let mut handlers: Vec<StepOutcome> = Vec::new();

        let finish = |status: ModuleStatus,
                      error: Option<ErrorReport>,
                      changes: usize,
                      health: Vec<HealthResult>,
                      blocked: Vec<String>,
                      steps: Vec<StepOutcome>,
                      handlers: Vec<StepOutcome>| ModuleRun {
            failed: matches!(status, ModuleStatus::Failed | ModuleStatus::Unhealthy),
            blocked,
            outcome: ModuleOutcome {
                module: module.clone(),
                status,
                changes,
                duration_ms: started.elapsed().as_millis() as u64,
                error: error.as_ref().map(ErrorReport::one_line),
                error_detail: error,
                steps,
                handlers,
                started_at: Some(started_at),
                finished_at: Some(self.inputs.system.now()),
                stale: false,
            },
            health,
        };

        for (index, step) in rich.plan.steps.iter().enumerate() {
            let rs = &rich.steps[index];
            let id = super::step_id(&step.resource);
            let span = tracing::info_span!(
                "task.apply",
                buckle.module = %module,
                buckle.task.kind = %step.task.kind,
                buckle.task.key = %step.resource.key,
                buckle.changed = tracing::field::Empty,
            );
            let step_started_at = self.inputs.system.now();
            let step_clock = tokio::time::Instant::now();
            let replanned: Option<Planned> = match &step.planned {
                Planned::Blocked { .. } if self.changed_so_far.load(Ordering::Relaxed) > 0 => {
                    let mut cx = rs.cx.clone();
                    cx.previous = self
                        .with_state(|s| s.get(&step.resource).cloned())
                        .map(Arc::new);
                    let replan = rs
                        .task
                        .plan(&cx, &rs.spec)
                        .instrument(tracing::info_span!(
                            "task.plan",
                            buckle.module = %module,
                            buckle.task.kind = %step.task.kind,
                            buckle.task.key = %step.resource.key,
                        ))
                        .await;
                    match replan {
                        Ok(Planned::Change { diff }) if diff.is_destructive() => {
                            tracing::warn!(parent: &span, "the blocked step now plans a deletion, which was never reviewed; leaving it blocked");
                            Some(Planned::blocked(format!(
                                "once unblocked it plans a deletion ({}), which needs a reviewed plan",
                                diff.summary()
                            )))
                        }
                        Ok(planned) => {
                            tracing::info!(parent: &span, change = planned.is_change(), "planned the blocked step again after earlier changes");
                            Some(planned)
                        }
                        Err(err) => {
                            tracing::warn!(parent: &span, error = %self.scrub(&err.description()), "planning the blocked step again failed; leaving it blocked");
                            None
                        }
                    }
                }
                _ => None,
            };
            let planned = replanned.as_ref().unwrap_or(&step.planned);
            let outcome = &mut steps[index];
            outcome.started_at = Some(step_started_at);
            match planned {
                Planned::Blocked { reason } => {
                    span.record("buckle.changed", false);
                    blocked.push(format!("{} is blocked: {reason}", step.resource));
                    outcome.state = StepState::Blocked;
                    outcome.result = Some(format!("blocked: {reason}"));
                    self.emit(ApplyEvent::StepBlocked {
                        module: module.clone(),
                        task_id: step.task.id.clone(),
                        id,
                        kind: step.task.kind.clone(),
                        reason: reason.clone(),
                    })
                    .await;
                }
                Planned::NoOp => {
                    span.record("buckle.changed", false);
                    self.record_step(step, rs, None, revision);
                    self.checkpoint();
                    outcome.state = StepState::Unchanged;
                    outcome.result = Some("no change".to_owned());
                    self.emit(ApplyEvent::StepFinished {
                        module: module.clone(),
                        task_id: step.task.id.clone(),
                        id,
                        kind: step.task.kind.clone(),
                        changed: false,
                        summary: "no change".to_owned(),
                    })
                    .await;
                }
                Planned::Change { .. } => {
                    match self
                        .apply_step(&module, step, rs, planned, revision, &span)
                        .await
                    {
                        Ok(applied) => {
                            let outcome = &mut steps[index];
                            outcome.state = if applied.changed {
                                StepState::Changed
                            } else {
                                StepState::Unchanged
                            };
                            outcome.result = Some(self.scrub(&applied.summary));
                            if applied.changed {
                                changes += 1;
                                if !step.notifies.is_empty() {
                                    changed_steps.push(step.clone());
                                }
                            }
                        }
                        Err(report) => {
                            let outcome = &mut steps[index];
                            outcome.state = StepState::Failed;
                            outcome.finished_at = Some(self.inputs.system.now());
                            outcome.duration_ms = Some(step_clock.elapsed().as_millis() as u64);
                            outcome.error_detail = Some(report.clone());
                            // The handlers it would have notified never run.
                            handlers.extend(
                                rich.plan
                                    .handlers
                                    .iter()
                                    .map(|h| StepOutcome::planned_handler(h, StepState::NotRun)),
                            );
                            self.emit(ApplyEvent::ModuleFinished {
                                module: module.clone(),
                                status: ModuleStatus::Failed,
                            })
                            .await;
                            return finish(
                                ModuleStatus::Failed,
                                Some(report.prefixed(&format!("{} failed", step.resource))),
                                changes,
                                health,
                                blocked,
                                steps,
                                handlers,
                            );
                        }
                    }
                }
            }
            let outcome = &mut steps[index];
            outcome.finished_at = Some(self.inputs.system.now());
            outcome.duration_ms = Some(step_clock.elapsed().as_millis() as u64);
        }

        // Handlers notified by the steps that really changed, plus those other
        // modules left for this one; a handler of a module that runs later in
        // this apply (it needs this one) waits for the end of that module.
        let mut local: Vec<(String, String)> = Vec::new();
        for (owner, handler) in super::planner::notifications(&module, changed_steps.iter()) {
            if owner != module && self.runs_later(planner, &owner, &module) {
                self.pending_handlers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(owner.clone())
                    .or_default()
                    .push((owner, handler));
            } else if !local.contains(&(owner.clone(), handler.clone())) {
                local.push((owner, handler));
            }
        }
        for n in self.take_pending(&module) {
            if !local.contains(&n) {
                local.push(n);
            }
        }
        let (ran, result) = self
            .run_notified(planner, &module, Some(&rich.ctx), &local, contexts)
            .await;
        handlers.extend(ran);
        // Planned handlers that nothing notified in the end (the steps that
        // notify them changed nothing): skipped, and said so.
        for planned in &rich.plan.handlers {
            let id = super::handler_id(&planned.module, &planned.handler);
            if handlers.iter().any(|h| h.id == id) {
                continue;
            }
            let state = if result.is_err() {
                StepState::NotRun
            } else {
                StepState::Skipped
            };
            let mut outcome = StepOutcome::planned_handler(planned, state);
            if state == StepState::Skipped {
                let reason = "not notified: the steps that notify it changed nothing".to_owned();
                self.emit(ApplyEvent::StepSkipped {
                    module: module.clone(),
                    task_id: id.clone(),
                    id,
                    kind: planned.kind.clone(),
                    reason: reason.clone(),
                })
                .await;
                outcome.result = Some(reason);
            }
            handlers.push(outcome);
        }
        if let Err(report) = result {
            self.emit(ApplyEvent::ModuleFinished {
                module: module.clone(),
                status: ModuleStatus::Failed,
            })
            .await;
            return finish(
                ModuleStatus::Failed,
                Some(report),
                changes,
                health,
                blocked,
                steps,
                handlers,
            );
        }

        let mut status = if !blocked.is_empty() {
            ModuleStatus::Blocked
        } else if changes > 0 {
            ModuleStatus::Applied
        } else {
            ModuleStatus::Unchanged
        };
        let mut error = (!blocked.is_empty()).then(|| {
            ErrorReport::user(format!("needs attention: {}", blocked.join("; "))).with_advice([
                    "Install or enable what the blocked steps are missing, then apply again; a blocked step is planned again once earlier steps have changed something.",
                ])
        });
        let config = planner.revision.module(&module).map(|m| &m.config);
        // Providers the module declares re-run now, so that its own health
        // checks (and the modules planned after it) see what it changed.
        if let Some(config) = config
            && !config.facts.provides.is_empty()
        {
            self.refresh_facts(planner, &config.facts.provides).await;
        }
        // Once the run has asked for a reboot, checks would judge the host
        // before the change takes effect (and a failure would roll the
        // module back): they run after the reboot instead, in the bake.
        let deferred_by_reboot =
            run_health && config.is_some_and(|c| !c.health.is_empty()) && self.reboot_pending();
        if deferred_by_reboot {
            tracing::info!(module = %module, "health checks deferred until after the reboot");
        }
        if run_health
            && !deferred_by_reboot
            && let Some(config) = config
            && !config.health.is_empty()
        {
            // Checks evaluate against the facts as refreshed so far, not the
            // plan-time snapshot.
            let contexts = planner.contexts(&self.facts());
            let ctx = planner
                .module_context(&contexts, config)
                .unwrap_or_else(|_| rich.ctx.clone());
            let module_checks = checks::ModuleChecks {
                module: &module,
                decls: &config.health,
                ctx: &ctx,
                source: &rich.source,
            };
            if changes == 0 {
                // Nothing changed, so nothing this run did can have broken
                // the module: report its checks without gating on them.
                let results = checks::run_once(planner, &module_checks).await;
                checks::report(&module, &results, &self.events).await;
                health = results;
                self.emit(ApplyEvent::ModuleFinished {
                    module: module.clone(),
                    status,
                })
                .await;
                return finish(status, error, changes, health, blocked, steps, handlers);
            }
            let (results, healthy) = checks::run_settled(
                planner,
                &module_checks,
                planner.revision.fleet.rollout.settle,
                &self.inputs.waived_checks,
                &self.events,
            )
            .await;
            if !healthy {
                let failing: Vec<String> = results
                    .iter()
                    .filter(|r| r.counts_as_failure(&self.inputs.waived_checks))
                    .map(|r| match &r.status {
                        crate::health::HealthStatus::Fail { reason } => {
                            format!("{} ({reason})", r.check)
                        }
                        _ => r.check.clone(),
                    })
                    .collect();
                status = ModuleStatus::Unhealthy;
                error = Some(
                    ErrorReport::user(format!(
                        "required health checks still failing after the settle window: {}",
                        failing.join(", ")
                    ))
                    .with_advice([
                        "Look at the failing checks on the host; the module was rolled back where a stable revision was available.",
                    ]),
                );
            }
            health = results;
        }
        self.emit(ApplyEvent::ModuleFinished {
            module: module.clone(),
            status,
        })
        .await;
        finish(status, error, changes, health, blocked, steps, handlers)
    }

    /// Rolls `module` back to the stable revision with the current facts:
    /// evaluates the stable revision's selection for this host, re-applies
    /// the module from it when the stable revision selects it, and removes the
    /// module's resources that the stable revision does not want (unless a
    /// resource of the stable revision took them over, or removals are
    /// disabled). `Ok(None)` when there is no stable revision or the stable
    /// selection of the module is indeterminate.
    async fn rollback(&self, module: &str, facts: &Facts) -> crate::Result<Option<ModuleRun>> {
        let Some(stable) = self.inputs.stable_revision.as_ref() else {
            return Ok(None);
        };
        let started = tokio::time::Instant::now();
        let mut planner = Planner::from_inputs(self.inputs, stable)?;
        planner.drift = false;
        let state = self.with_state(|s| s.clone());
        let contexts = planner.contexts(facts);
        let selection = planner.select(&contexts)?;
        if let Some(i) = selection.indeterminate.iter().find(|i| i.module == module) {
            tracing::warn!(buckle.module = module, reason = %i.reason, "the stable revision's selection of the module is indeterminate; not rolling back");
            return Ok(None);
        }
        // The whole stable selection's desired set, so that a resource another
        // module of the stable revision owns is never removed.
        let mut prepared = Vec::new();
        let mut complete = true;
        for m in &selection.order {
            let attempt = planner
                .module_context(&contexts, &m.config)
                .and_then(|ctx| planner.prepare(m, ctx, &contexts, false, &state));
            match attempt {
                Ok(p) => prepared.push(p),
                Err(err) if m.config.name == module => return Err(err),
                Err(err) => {
                    tracing::warn!(buckle.module = %m.config.name, error = %self.scrub(&err.message()), "a module of the stable revision could not be prepared; the rollback removes nothing");
                    complete = false;
                }
            }
        }
        let others: BTreeSet<String> = state
            .resources
            .values()
            .map(|r| r.key.module.clone())
            .filter(|m| m != module)
            .collect();
        let rehomed = planner.rehomes(&state, &mut prepared, &others);
        let skip: BTreeSet<_> = rehomed.iter().map(|r| r.from.clone()).collect();
        let desired: Vec<_> = prepared
            .iter()
            .flat_map(|p| p.desired_keys().cloned())
            .collect();
        let removals: Vec<_> = if complete && self.inputs.removals {
            planner.removals(&state, &desired, &others, &skip)
        } else {
            Vec::new()
        };

        let span = tracing::info_span!("module.rollback", buckle.module = %module, buckle.revision = %stable.id);
        let mut run = match prepared.into_iter().find(|p| p.name == module) {
            Some(target) => {
                let rich = planner.plan_module(target, &contexts).await?;
                self.run_module(&planner, Arc::new(rich), &contexts, &stable.id, true)
                    .instrument(span.clone())
                    .await
            }
            // The stable revision does not select the module here: rolling
            // back means removing what the failed revision put in place.
            None => ModuleRun {
                outcome: ModuleOutcome::new(module, ModuleStatus::RolledBack),
                health: Vec::new(),
                failed: false,
                blocked: Vec::new(),
            },
        };
        if run.failed {
            return Ok(Some(run));
        }
        for r in &rehomed {
            self.with_state(|s| {
                if let Some(mut record) = s.forget(&r.from)
                    && r.kind == crate::plan::RehomeKind::Moved
                    && s.get(&r.to).is_none()
                {
                    record.key = r.to.clone();
                    s.record(record);
                }
            });
        }
        let (plans, riches): (Vec<_>, Vec<_>) = removals.into_iter().unzip();
        let (_, errors, _) = self
            .run_removals(&planner, &contexts, &plans, &riches)
            .instrument(span)
            .await;
        if !errors.is_empty() {
            run.failed = true;
            run.outcome.status = ModuleStatus::Failed;
            let error = combine(errors).unwrap_or_else(|| ErrorReport::system("removals failed"));
            run.outcome.error = Some(error.one_line());
            run.outcome.error_detail = Some(error);
            return Ok(Some(run));
        }
        run.outcome.status = ModuleStatus::RolledBack;
        run.outcome.duration_ms = started.elapsed().as_millis() as u64;
        Ok(Some(run))
    }

    /// Runs removals in order. Returns how many succeeded, the errors and
    /// what happened to each removal.
    async fn run_removals(
        &self,
        planner: &Planner<'_>,
        contexts: &Contexts,
        removals: &[crate::plan::Removal],
        rich: &[super::planner::RichRemoval],
    ) -> (usize, Vec<ErrorReport>, Vec<RemovalOutcome>) {
        let mut removed = 0usize;
        let mut errors = Vec::new();
        let mut outcomes = Vec::with_capacity(removals.len());
        let mut notified: Vec<(String, String)> = Vec::new();
        for (removal, rr) in removals.iter().zip(rich) {
            let span = tracing::info_span!("removal.run", buckle.resource = %removal.resource, buckle.policy = ?removal.policy);
            let outcome: Result<(String, bool), ErrorReport> = async {
                match (removal.policy, rr.task) {
                    (RemovalPolicy::Leave, _) => Ok(("released: left in place (policy leave)".to_owned(), false)),
                    (RemovalPolicy::Cleanup, None) => Ok((format!(
                        "forgotten: the task kind '{}' is no longer available, nothing was cleaned up",
                        removal.resource.kind
                    ), false)),
                    (RemovalPolicy::Cleanup, Some(task)) => {
                        let cx = planner.removal_cx(contexts, &rr.record);
                        let _guards = self.locks.acquire(&task.locks().iter().map(|l| l.to_string()).collect::<Vec<_>>()).await;
                        task.remove(&cx, &rr.record)
                            .await
                            .map(|a| (self.scrub(&a.summary), a.changed))
                            .map_err(|e| self.report(&e))
                    }
                }
            }
            .instrument(span)
            .await;
            match outcome {
                Ok((summary, changed)) => {
                    let notes = self.with_state(|s| {
                        let notes = s.notifies_of(&removal.resource).to_vec();
                        s.forget(&removal.resource);
                        notes
                    });
                    self.checkpoint();
                    if changed {
                        for note in notes {
                            let (m, h) =
                                super::planner::qualify_handler(&removal.resource.module, &note);
                            // Handlers of a module that left the revision are gone.
                            let exists = planner
                                .revision
                                .module(&m)
                                .is_some_and(|owner| owner.config.handlers.contains_key(&h));
                            if exists && !notified.contains(&(m.clone(), h.clone())) {
                                notified.push((m, h));
                            }
                        }
                    }
                    removed += 1;
                    outcomes.push(RemovalOutcome {
                        resource: removal.resource.to_string(),
                        kind: removal.resource.kind.clone(),
                        summary: summary.clone(),
                        state: if changed {
                            StepState::Changed
                        } else {
                            StepState::Unchanged
                        },
                        caution: removal.caution,
                        error_detail: None,
                    });
                    self.emit(ApplyEvent::RemovalRun {
                        resource: removal.resource.to_string(),
                        summary,
                    })
                    .await;
                }
                Err(report) => {
                    self.emit(ApplyEvent::RemovalRun {
                        resource: removal.resource.to_string(),
                        summary: format!("failed: {}", report.one_line()),
                    })
                    .await;
                    outcomes.push(RemovalOutcome {
                        resource: removal.resource.to_string(),
                        kind: removal.resource.kind.clone(),
                        summary: removal.summary.clone(),
                        state: StepState::Failed,
                        caution: removal.caution,
                        error_detail: Some(report.clone()),
                    });
                    errors.push(report.prefixed(&format!("removing {} failed", removal.resource)));
                }
            }
        }
        // The handlers the removed tasks notified run once, after the removals.
        if let (_, Err(report)) = self
            .run_notified(planner, "removals", None, &notified, contexts)
            .await
        {
            errors.push(report);
        }
        (removed, errors, outcomes)
    }

    async fn run(
        &self,
        planner: &Planner<'_>,
        rich: RichPlan,
        started: tokio::time::Instant,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> crate::Result<ApplyOutcome> {
        let revision = planner.revision.id.clone();
        let plan_digest = rich.plan.digest.clone();
        let parallelism = self.inputs.parallelism.max(1);

        // Records a desired resource took over: re-key moved ones, forget
        // superseded ones (without cleanup) so that nothing removes them.
        for r in &rich.plan.rehomed {
            self.with_state(|s| {
                if let Some(mut record) = s.forget(&r.from)
                    && r.kind == crate::plan::RehomeKind::Moved
                    && s.get(&r.to).is_none()
                {
                    record.key = r.to.clone();
                    s.record(record);
                }
            });
        }

        let mut module_plans: HashMap<String, Arc<RichModulePlan>> = rich
            .modules
            .into_iter()
            .map(|m| (m.plan.module.clone(), Arc::new(m)))
            .collect();
        let deferred: HashSet<String> = rich
            .plan
            .deferred
            .iter()
            .map(|d| d.module.clone())
            .collect();
        // What each previewed deferred module was approved to do.
        let previews: HashMap<String, crate::Digest> = rich
            .plan
            .deferred
            .iter()
            .filter_map(|d| Some((d.module.clone(), d.preview.as_ref()?.intent_digest())))
            .collect();
        let order = rich.order.clone();
        let needs = rich.needs.clone();

        let mut started_modules: HashSet<String> = HashSet::new();
        let mut done: HashSet<String> = HashSet::new();
        let mut running: FuturesUnordered<ModuleFuture<'_>> = FuturesUnordered::new();
        let mut results: Vec<ModuleOutcome> = Vec::new();
        let mut health: Vec<HealthResult> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut halted = false;
        let mut total_changes = 0usize;
        let mut errors: Vec<ErrorReport> = Vec::new();
        // Modules that are selected but could not be planned fail the run
        // without halting it (nothing of theirs ran); indeterminate ones are
        // reported and left alone.
        let mut plan_errors: Vec<String> = Vec::new();
        // Modules that finished with blocked steps: the run needs attention.
        let mut blocked_modules: Vec<String> = Vec::new();
        // Orphans of deferred modules, known once they are planned.
        let mut late_removals: Vec<(crate::plan::Removal, super::planner::RichRemoval)> =
            Vec::new();
        for i in &rich.plan.indeterminate {
            let (status, error) = if i.plan_error {
                let detail = i
                    .error_detail
                    .clone()
                    .unwrap_or_else(|| ErrorReport::user(i.reason.clone()));
                (ModuleStatus::Failed, detail.prefixed("planning failed"))
            } else {
                (
                    ModuleStatus::Indeterminate,
                    ErrorReport::user(i.reason.clone()),
                )
            };
            results.push(ModuleOutcome::new(&i.module, status).with_error(error));
            if i.plan_error {
                plan_errors.push(i.module.clone());
                self.with_state(|s| {
                    s.failed_modules.insert(i.module.clone(), revision.clone());
                });
            }
        }

        loop {
            if !halted {
                for name in &order {
                    if running.len() >= parallelism {
                        break;
                    }
                    if started_modules.contains(name) || done.contains(name) {
                        continue;
                    }
                    let ready = needs
                        .get(name)
                        .is_none_or(|deps| deps.iter().all(|d| done.contains(d)));
                    if !ready {
                        continue;
                    }
                    if !module_plans.contains_key(name) && deferred.contains(name) {
                        let state = self.with_state(|s| s.clone());
                        let planned = planner
                            .plan_deferred(name, &self.facts(), &state, &rich.desired)
                            .await;
                        // A previewed module must do what its preview showed:
                        // otherwise nothing of it runs, and the host is
                        // re-planned for approval.
                        let diverged = previews.get(name).and_then(|approved| match &planned {
                            Ok((DeferredPlan::Planned(p), _)) => {
                                (p.plan.intent_digest() != *approved).then_some(
                                    "its plan now does something other than its approved preview",
                                )
                            }
                            Ok((DeferredPlan::Unselected, _)) => {
                                Some("it is no longer selected, unlike its approved preview")
                            }
                            _ => None,
                        });
                        if let Some(why) = diverged {
                            tracing::warn!(buckle.module = %name, why, "a deferred module diverged from its approved preview");
                            started_modules.insert(name.clone());
                            let mut outcome = ModuleOutcome::new(name, ModuleStatus::Failed)
                                .with_error(ErrorReport::user(format!(
                                    "not applied: {why}; re-plan the host and approve it again"
                                )));
                            outcome.stale = true;
                            results.push(outcome);
                            plan_errors.push(name.clone());
                            continue;
                        }
                        match planned {
                            Ok((DeferredPlan::Planned(planned), orphans)) => {
                                module_plans.insert(name.clone(), Arc::new(planned));
                                late_removals.extend(orphans);
                            }
                            Ok((DeferredPlan::Unselected, orphans)) => {
                                // Not selected after all: its resources are
                                // orphans; its dependents cannot run.
                                started_modules.insert(name.clone());
                                late_removals.extend(orphans);
                                results.push(ModuleOutcome::new(name, ModuleStatus::Skipped));
                                continue;
                            }
                            Ok((DeferredPlan::Indeterminate(reason), _)) => {
                                started_modules.insert(name.clone());
                                results.push(
                                    ModuleOutcome::new(name, ModuleStatus::Indeterminate)
                                        .with_error(ErrorReport::user(reason)),
                                );
                                continue;
                            }
                            Err(err) => {
                                let report = self.report(&err);
                                tracing::error!(buckle.module = %name, error = %report.message, causes = ?report.causes, "planning the deferred module failed");
                                // Nothing of it ran, so there is nothing to
                                // roll back; its dependents never become ready.
                                started_modules.insert(name.clone());
                                results.push(
                                    ModuleOutcome::new(name, ModuleStatus::Failed)
                                        .with_error(report.prefixed("planning failed")),
                                );
                                plan_errors.push(name.clone());
                                self.with_state(|s| {
                                    s.failed_modules.insert(name.clone(), revision.clone());
                                });
                                continue;
                            }
                        }
                    }
                    let Some(module_plan) = module_plans.get(name).cloned() else {
                        continue;
                    };
                    started_modules.insert(name.clone());
                    self.scheduled
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(name);
                    let rev = revision.clone();
                    let contexts = Arc::new(planner.contexts(&self.facts()));
                    let span = tracing::info_span!("module.apply", buckle.module = %name);
                    running.push(Box::pin(
                        async move {
                            self.run_module(planner, module_plan, &contexts, &rev, true)
                                .await
                        }
                        .instrument(span),
                    ));
                }
            }
            let Some(run) = running.next().await else {
                break;
            };
            let name = run.outcome.module.clone();
            done.insert(name.clone());
            total_changes += run.outcome.changes;
            health.extend(run.health);
            self.with_state(|s| {
                if run.failed {
                    s.failed_modules.insert(name.clone(), revision.clone());
                } else {
                    s.failed_modules.remove(&name);
                    s.module_revisions.insert(name.clone(), revision.clone());
                }
            });
            if run.failed {
                failed.push(name.clone());
                halted = true;
            } else if !run.blocked.is_empty() {
                blocked_modules.push(name.clone());
            }
            results.push(run.outcome);
            let facts = self.facts();
            self.with_state(|s| s.facts = facts);
            self.checkpoint();
        }

        // Module-scoped rollback of whatever failed, after in-flight modules drained.
        let mut rolled_back = Vec::new();
        for name in &failed {
            match self.rollback(name, &self.facts()).await {
                Ok(Some(run)) => {
                    health.extend(run.health);
                    if run.outcome.status == ModuleStatus::RolledBack {
                        rolled_back.push(name.clone());
                        if let Some(stable) = &self.inputs.stable_revision {
                            self.with_state(|s| {
                                s.module_revisions.insert(name.clone(), stable.id.clone());
                            });
                        }
                    } else {
                        errors.push(
                            run.outcome
                                .error_detail
                                .clone()
                                .unwrap_or_else(|| ErrorReport::system("the rollback failed"))
                                .prefixed(&format!("rolling back '{name}' failed")),
                        );
                    }
                    if let Some(existing) = results.iter_mut().find(|r| &r.module == name) {
                        let original_error = existing.error.take();
                        existing.status = run.outcome.status;
                        existing.error = match (original_error, run.outcome.error.clone()) {
                            (Some(o), Some(r)) => Some(format!("{o}; rollback: {r}")),
                            (o, r) => o.or(r),
                        };
                        existing.error_detail =
                            match (existing.error_detail.take(), run.outcome.error_detail) {
                                (Some(o), Some(r)) => {
                                    Some(o.with_causes([format!("rollback: {}", r.message)]))
                                }
                                (o, r) => o.or(r),
                            };
                        if run.outcome.status == ModuleStatus::RolledBack {
                            // What this run applied of the module was undone.
                            for step in existing
                                .steps
                                .iter_mut()
                                .chain(existing.handlers.iter_mut())
                            {
                                if matches!(step.state, StepState::Changed | StepState::Unchanged) {
                                    step.state = StepState::RolledBack;
                                }
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => errors.push(
                    self.report(&err)
                        .prefixed(&format!("rolling back '{name}' failed")),
                ),
            }
            self.checkpoint();
        }

        // Handlers left for modules that never ran (the run halted, or they
        // turned out unselected) still run: the changes that notified them
        // were made.
        let leftovers: Vec<(String, Vec<(String, String)>)> = {
            let mut map = std::mem::take(
                &mut *self
                    .pending_handlers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()),
            );
            let mut owners: Vec<String> = map.keys().cloned().collect();
            owners.sort();
            owners
                .into_iter()
                .map(|o| {
                    let notes = map.remove(&o).unwrap_or_default();
                    (o, notes)
                })
                .collect()
        };
        // What the leftover handlers did, by owner, for owners that have no
        // outcome yet (they never started: their outcome is built below).
        let mut leftover_ran: HashMap<String, Vec<StepOutcome>> = HashMap::new();
        if !leftovers.is_empty() {
            let contexts = planner.contexts(&self.facts());
            for (owner, notes) in leftovers {
                let (ran, result) = self
                    .run_notified(planner, &owner, None, &notes, &contexts)
                    .await;
                match results.iter_mut().find(|r| r.module == owner) {
                    Some(existing) => merge_handlers(&mut existing.handlers, ran),
                    None => leftover_ran.entry(owner).or_default().extend(ran),
                }
                if let Err(report) = result {
                    errors.push(report);
                }
            }
        }

        let not_started: Vec<String> = order
            .iter()
            .filter(|n| !done.contains(*n) && !results.iter().any(|r| &r.module == *n))
            .cloned()
            .collect();
        for name in &not_started {
            let mut outcome = ModuleOutcome::new(name, ModuleStatus::NotStarted);
            // Its planned steps never ran (a deferred module has none yet).
            if let Some(plan) = module_plans.get(name) {
                outcome.steps = plan
                    .plan
                    .steps
                    .iter()
                    .map(|s| StepOutcome::planned(s, StepState::NotRun))
                    .collect();
                outcome.handlers = plan
                    .plan
                    .handlers
                    .iter()
                    .map(|h| StepOutcome::planned_handler(h, StepState::NotRun))
                    .collect();
            }
            // Handlers other modules' changes notified ran anyway.
            if let Some(ran) = leftover_ran.remove(name) {
                merge_handlers(&mut outcome.handlers, ran);
            }
            results.push(outcome);
        }
        // Owners that are neither in the plan's order nor reported (none
        // expected): keep what their handlers did rather than drop it.
        for (owner, ran) in leftover_ran {
            let mut outcome = ModuleOutcome::new(owner, ModuleStatus::NotStarted);
            outcome.handlers = ran;
            results.push(outcome);
        }

        let mut removed = 0usize;
        // What happens to each planned removal: run, or left alone (the run
        // halted, or removals are disabled for it).
        let mut removals_run: Vec<RemovalOutcome> = Vec::new();
        if halted || !self.inputs.removals {
            let state = if halted {
                StepState::NotRun
            } else {
                StepState::Skipped
            };
            removals_run.extend(
                rich.plan
                    .removals
                    .iter()
                    .chain(late_removals.iter().map(|(r, _)| r))
                    .map(|r| RemovalOutcome {
                        resource: r.resource.to_string(),
                        kind: r.resource.kind.clone(),
                        summary: r.summary.clone(),
                        state,
                        caution: r.caution,
                        error_detail: None,
                    }),
            );
        }
        if !halted && !self.inputs.removals && !rich.plan.removals.is_empty() {
            tracing::info!(
                skipped = rich.plan.removals.len(),
                "removals are disabled for this run; the planned removals were left in place"
            );
        }
        if !halted && self.inputs.removals {
            let contexts = planner.contexts(&self.facts());
            let (count, removal_errors, ran) = self
                .run_removals(planner, &contexts, &rich.plan.removals, &rich.removals)
                .await;
            removed = count;
            errors.extend(removal_errors);
            removals_run.extend(ran);
            let (plans, riches): (Vec<_>, Vec<_>) =
                std::mem::take(&mut late_removals).into_iter().unzip();
            let (count, removal_errors, ran) =
                self.run_removals(planner, &contexts, &plans, &riches).await;
            removed += count;
            errors.extend(removal_errors);
            removals_run.extend(ran);
        }

        let succeeded =
            !halted && errors.is_empty() && plan_errors.is_empty() && blocked_modules.is_empty();
        let failed_module = failed.first().or(plan_errors.first()).cloned();
        let error = {
            let mut parts: Vec<String> = results
                .iter()
                .filter(|r| {
                    failed.contains(&r.module)
                        || plan_errors.contains(&r.module)
                        || blocked_modules.contains(&r.module)
                })
                .filter_map(|r| {
                    r.error
                        .clone()
                        .map(|e| format!("module '{}': {e}", r.module))
                })
                .collect();
            parts.extend(errors.iter().map(ErrorReport::one_line));
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("; "))
            }
        };
        let run_outcome = if succeeded {
            RunOutcome::Succeeded
        } else if !rolled_back.is_empty() {
            RunOutcome::RolledBack
        } else if results
            .iter()
            .any(|r| failed.contains(&r.module) && r.status == ModuleStatus::Unhealthy)
        {
            RunOutcome::Unhealthy
        } else if failed.is_empty() && plan_errors.is_empty() && errors.is_empty() {
            RunOutcome::NeedsAttention
        } else {
            RunOutcome::Failed
        };
        let finished_at = self.inputs.system.now();
        let facts = self.facts();
        self.with_state(|s| {
            s.facts = facts;
            if succeeded {
                s.applied_revision = Some(revision.clone());
                // Forget modules that are neither selected nor own anything.
                let owners: BTreeSet<String> =
                    s.resources.values().map(|r| r.key.module.clone()).collect();
                s.module_revisions
                    .retain(|m, _| order.contains(m) || owners.contains(m));
            }
            s.push_run(
                RunRecord {
                    revision: revision.clone(),
                    plan_digest: plan_digest.clone(),
                    started_at,
                    finished_at,
                    outcome: run_outcome,
                    changes: total_changes,
                    removals: removed,
                    failed_module: failed_module.clone(),
                    rolled_back_modules: rolled_back.clone(),
                    error: error.clone(),
                },
                50,
            );
        });
        self.checkpoint();
        // A failed save never turns what happened on the host into an error;
        // it is reported alongside the outcome.
        let save_errors =
            std::mem::take(&mut *self.save_errors.lock().unwrap_or_else(|p| p.into_inner()));
        let error = if save_errors.is_empty() {
            error
        } else {
            let saves = save_errors
                .iter()
                .map(ErrorReport::one_line)
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!(error = %saves, "the state file could not be saved");
            Some(match error {
                Some(e) => format!("{e}; {saves}"),
                None => saves,
            })
        };
        // The run-level error: what no single module accounts for.
        errors.extend(save_errors);
        let error_detail = combine(errors);

        let deferred_names: BTreeSet<&String> = deferred.iter().collect();
        tracing::info!(
            succeeded,
            changes = total_changes,
            removals = removed,
            deferred = deferred_names.len(),
            "apply finished"
        );
        let outcome = ApplyOutcome {
            plan_digest,
            succeeded,
            changes: total_changes,
            removals: removed,
            duration_ms: started.elapsed().as_millis() as u64,
            module_results: results,
            failed_module,
            rolled_back_modules: rolled_back,
            not_started,
            health: Some(HealthReport {
                scope: HealthScope::Bake,
                results: health,
            }),
            error,
            error_detail,
            removals_run,
            started_at: Some(started_at),
            finished_at: Some(finished_at),
            reboot: self.reboot.lock().unwrap_or_else(|p| p.into_inner()).take(),
        };
        // For `buckle status`; never fails the apply.
        if let Err(e) = self.inputs.state.save_last_run(&outcome) {
            tracing::warn!(error = %e.description(), "the outcome of the run could not be kept for `buckle status`");
        }
        Ok(outcome)
    }
}

/// Puts the outcomes of handlers that ran into `handlers`, replacing the
/// planned (not run) entry of the same handler.
fn merge_handlers(handlers: &mut Vec<StepOutcome>, ran: Vec<StepOutcome>) {
    for h in ran {
        match handlers.iter_mut().find(|x| x.id == h.id) {
            Some(existing) => *existing = h,
            None => handlers.push(h),
        }
    }
}

/// Several errors as one report: their messages joined, every cause and
/// piece of advice once. `None` when there are none; the error itself when
/// there is one.
fn combine(errors: Vec<ErrorReport>) -> Option<ErrorReport> {
    if errors.len() <= 1 {
        return errors.into_iter().next();
    }
    let kind = if errors
        .iter()
        .any(|e| e.kind == crate::error::ErrorKind::System)
    {
        crate::error::ErrorKind::System
    } else {
        crate::error::ErrorKind::User
    };
    let mut combined = ErrorReport {
        kind,
        message: errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; "),
        causes: Vec::new(),
        advice: Vec::new(),
    };
    for e in errors {
        for c in e.causes {
            if !combined.causes.contains(&c) {
                combined.causes.push(c);
            }
        }
        for a in e.advice {
            if !combined.advice.contains(&a) {
                combined.advice.push(a);
            }
        }
    }
    Some(combined)
}
