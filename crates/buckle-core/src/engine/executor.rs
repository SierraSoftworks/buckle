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
    Contexts, Planner, RichHandler, RichModulePlan, RichPlan, RichStep, collect_facts, plan_with,
};
use super::{ApplyEvent, ApplyOutcome, Inputs, ModuleOutcome, ModuleStatus};
use crate::Digest;
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
    let mut facts = collect_facts(inputs, &planner).await;
    let state = inputs.state.load()?;
    let rich = plan_with(&planner, &facts, &state).await?;
    if rich.plan.digest != plan.digest {
        return Err(crate::error::user(
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
    };
    let outcome = executor
        .run(&planner, rich, &mut facts, started, started_at)
        .await?;
    Ok(outcome)
}

impl<'a> Executor<'a> {
    async fn emit(&self, event: ApplyEvent) {
        let _ = self.events.send(event).await;
    }

    fn scrub(&self, text: &str) -> String {
        if self.redactor.is_empty() {
            text.to_owned()
        } else {
            self.redactor.redact(text).into_inner()
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut state)
    }

    fn save_state(&self) -> crate::Result<()> {
        let snapshot = self.with_state(|s| s.clone());
        self.inputs.state.save(&snapshot)
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
        let now = self.inputs.system.now();
        self.with_state(|state| {
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
                artefacts: match applied {
                    Some(a) => a.artefacts.clone(),
                    None => existing
                        .as_ref()
                        .map(|e| e.artefacts.clone())
                        .unwrap_or_default(),
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
    /// resource. `Err` carries the scrubbed error message.
    async fn apply_step(
        &self,
        module: &str,
        step: &Step,
        rs: &RichStep,
        planned: &Planned,
        revision: &RevisionId,
        span: &tracing::Span,
    ) -> Result<Applied, String> {
        self.emit(ApplyEvent::StepStarted {
            module: module.to_owned(),
            task_id: step.task.id.clone(),
            kind: step.task.kind.clone(),
        })
        .await;
        let mut cx = rs.cx.clone();
        cx.mode = Mode::Apply;
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
                self.record_step(step, rs, Some(&applied), revision);
                self.emit(ApplyEvent::StepFinished {
                    module: module.to_owned(),
                    task_id: step.task.id.clone(),
                    kind: step.task.kind.clone(),
                    changed: applied.changed,
                    summary: self.scrub(&applied.summary),
                })
                .await;
                Ok(applied)
            }
            Err(err) => {
                let message = self.scrub(&err.message());
                tracing::error!(parent: span, error = %message, "task failed");
                self.emit(ApplyEvent::StepFailed {
                    module: module.to_owned(),
                    task_id: step.task.id.clone(),
                    kind: step.task.kind.clone(),
                    error: message.clone(),
                })
                .await;
                Err(message)
            }
        }
    }

    /// Runs one handler (when its plan is a change) and emits the event.
    async fn run_handler(
        &self,
        module: &str,
        run: &HandlerRun,
        rh: &RichHandler,
    ) -> Result<(), String> {
        let span = tracing::info_span!(
            "handler.run",
            buckle.module = %run.module,
            buckle.handler = %run.handler,
            buckle.task.kind = %run.kind,
        );
        if rh.planned.is_change() {
            let mut cx = rh.cx.clone();
            cx.mode = Mode::Apply;
            let locks: Vec<String> = rh.task.locks().iter().map(|l| l.to_string()).collect();
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
            if let Err(err) = result {
                let message = self.scrub(&err.message());
                tracing::error!(parent: &span, error = %message, "handler failed");
                self.emit(ApplyEvent::StepFailed {
                    module: module.to_owned(),
                    task_id: format!("{}:{}", run.module, run.handler),
                    kind: run.kind.clone(),
                    error: message.clone(),
                })
                .await;
                return Err(format!(
                    "handler {}:{} failed: {message}",
                    run.module, run.handler
                ));
            }
        }
        self.emit(ApplyEvent::HandlerRun {
            module: run.module.clone(),
            handler: run.handler.clone(),
        })
        .await;
        Ok(())
    }

    /// Runs one module: steps in order, then handlers, then (optionally) the
    /// module's health checks with settle.
    ///
    /// A step that was `Blocked` at plan time by its own kind (not by the
    /// platform) is planned again once anything earlier in the run changed,
    /// because an earlier step may have installed the tool it was missing. If
    /// it now plans a change, the change is applied and the handlers it
    /// notifies run alongside the planned ones.
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
        self.emit(ApplyEvent::ModuleStarted {
            module: module.clone(),
        })
        .await;
        let mut changes = 0usize;
        let mut health = Vec::new();
        let mut late_changes: Vec<Step> = Vec::new();

        let finish = |status: ModuleStatus,
                      error: Option<String>,
                      changes: usize,
                      health: Vec<HealthResult>| ModuleRun {
            failed: matches!(status, ModuleStatus::Failed | ModuleStatus::Unhealthy),
            outcome: ModuleOutcome {
                module: module.clone(),
                status,
                changes,
                duration_ms: started.elapsed().as_millis() as u64,
                error,
            },
            health,
        };

        for (index, step) in rich.plan.steps.iter().enumerate() {
            let rs = &rich.steps[index];
            let span = tracing::info_span!(
                "task.apply",
                buckle.module = %module,
                buckle.task.kind = %step.task.kind,
                buckle.task.key = %step.resource.key,
                buckle.changed = tracing::field::Empty,
            );
            let replanned: Option<Planned> = match &step.planned {
                Planned::Blocked { .. }
                    if !rs.platform_blocked && self.changed_so_far.load(Ordering::Relaxed) > 0 =>
                {
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
                        Ok(planned) => {
                            tracing::info!(parent: &span, change = planned.is_change(), "planned the blocked step again after earlier changes");
                            Some(planned)
                        }
                        Err(err) => {
                            tracing::warn!(parent: &span, error = %self.scrub(&err.message()), "planning the blocked step again failed; leaving it blocked");
                            None
                        }
                    }
                }
                _ => None,
            };
            let planned = replanned.as_ref().unwrap_or(&step.planned);
            match planned {
                Planned::Blocked { reason } => {
                    span.record("buckle.changed", false);
                    self.emit(ApplyEvent::StepFinished {
                        module: module.clone(),
                        task_id: step.task.id.clone(),
                        kind: step.task.kind.clone(),
                        changed: false,
                        summary: format!("blocked: {reason}"),
                    })
                    .await;
                }
                Planned::NoOp => {
                    span.record("buckle.changed", false);
                    self.record_step(step, rs, None, revision);
                    self.emit(ApplyEvent::StepFinished {
                        module: module.clone(),
                        task_id: step.task.id.clone(),
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
                            if applied.changed {
                                changes += 1;
                                if replanned.is_some() && !step.notifies.is_empty() {
                                    late_changes.push(Step {
                                        planned: planned.clone(),
                                        ..step.clone()
                                    });
                                }
                            }
                        }
                        Err(message) => {
                            self.emit(ApplyEvent::ModuleFinished {
                                module: module.clone(),
                                status: ModuleStatus::Failed,
                            })
                            .await;
                            return finish(
                                ModuleStatus::Failed,
                                Some(format!("{} failed: {message}", step.resource)),
                                changes,
                                health,
                            );
                        }
                    }
                }
            }
        }

        // Handlers: the planned ones, then those notified only by steps that
        // were blocked at plan time and changed once planned again.
        let mut extra: Vec<(HandlerRun, RichHandler)> = Vec::new();
        if !late_changes.is_empty() {
            match planner
                .plan_handlers(&module, &rich.ctx, &late_changes, contexts)
                .await
            {
                Ok((runs, handlers)) => {
                    extra = runs
                        .into_iter()
                        .zip(handlers)
                        .filter(|(r, _)| {
                            !rich
                                .plan
                                .handlers
                                .iter()
                                .any(|h| h.module == r.module && h.handler == r.handler)
                        })
                        .collect();
                }
                Err(err) => {
                    let message = self.scrub(&err.message());
                    self.emit(ApplyEvent::ModuleFinished {
                        module: module.clone(),
                        status: ModuleStatus::Failed,
                    })
                    .await;
                    return finish(
                        ModuleStatus::Failed,
                        Some(format!("planning handlers failed: {message}")),
                        changes,
                        health,
                    );
                }
            }
        }
        let handlers = rich
            .plan
            .handlers
            .iter()
            .zip(rich.handlers.iter())
            .chain(extra.iter().map(|(r, h)| (r, h)));
        for (run, rh) in handlers {
            if let Err(message) = self.run_handler(&module, run, rh).await {
                self.emit(ApplyEvent::ModuleFinished {
                    module: module.clone(),
                    status: ModuleStatus::Failed,
                })
                .await;
                return finish(ModuleStatus::Failed, Some(message), changes, health);
            }
        }

        let mut status = if changes > 0 {
            ModuleStatus::Applied
        } else {
            ModuleStatus::Unchanged
        };
        let mut error = None;
        if run_health
            && let Some(config) = planner.revision.module(&module).map(|m| &m.config)
            && !config.health.is_empty()
        {
            let (results, healthy) = checks::run_settled(
                planner,
                &checks::ModuleChecks {
                    module: &module,
                    decls: &config.health,
                    ctx: &rich.ctx,
                    source: &rich.source,
                },
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
                error = Some(format!(
                    "required health checks still failing after the settle window: {}",
                    failing.join(", ")
                ));
            }
            health = results;
        }
        self.emit(ApplyEvent::ModuleFinished {
            module: module.clone(),
            status,
        })
        .await;
        finish(status, error, changes, health)
    }

    /// Re-plans and re-applies `module` from the stable revision with the
    /// current facts. `Ok(outcome)` carries `RolledBack` on success.
    async fn rollback(&self, module: &str, facts: &Facts) -> crate::Result<Option<ModuleRun>> {
        let Some(stable) = self.inputs.stable_revision.as_ref() else {
            return Ok(None);
        };
        if stable.module(module).is_none() {
            tracing::warn!(
                buckle.module = module,
                "the stable revision has no such module; nothing to roll back to"
            );
            return Ok(None);
        }
        let planner = Planner::from_inputs(self.inputs, stable)?;
        let state = self.with_state(|s| s.clone());
        let rich = planner.plan_one(module, facts, &state, false).await?;
        let contexts = planner.contexts(facts);
        let mut run = self
            .run_module(&planner, Arc::new(rich), &contexts, &stable.id, true)
            .instrument(tracing::info_span!("module.rollback", buckle.module = %module, buckle.revision = %stable.id))
            .await;
        if !run.failed {
            run.outcome.status = ModuleStatus::RolledBack;
        }
        Ok(Some(run))
    }

    async fn run(
        &self,
        planner: &Planner<'_>,
        rich: RichPlan,
        facts: &mut Facts,
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
        let order = rich.order.clone();
        let needs = rich.needs.clone();
        let provides = rich.provides.clone();

        let mut started_modules: HashSet<String> = HashSet::new();
        let mut done: HashSet<String> = HashSet::new();
        let mut running: FuturesUnordered<ModuleFuture<'_>> = FuturesUnordered::new();
        let mut results: Vec<ModuleOutcome> = Vec::new();
        let mut health: Vec<HealthResult> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut halted = false;
        let mut total_changes = 0usize;
        let mut errors: Vec<String> = Vec::new();
        // Modules that are selected but could not be planned fail the run
        // without halting it (nothing of theirs ran); indeterminate ones are
        // reported and left alone.
        let mut plan_errors: Vec<String> = Vec::new();
        for i in &rich.plan.indeterminate {
            results.push(ModuleOutcome {
                module: i.module.clone(),
                status: if i.plan_error {
                    ModuleStatus::Failed
                } else {
                    ModuleStatus::Indeterminate
                },
                changes: 0,
                duration_ms: 0,
                error: Some(if i.plan_error {
                    format!("planning failed: {}", i.reason)
                } else {
                    i.reason.clone()
                }),
            });
            if i.plan_error {
                plan_errors.push(i.module.clone());
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
                        match planner.plan_one(name, facts, &state, true).await {
                            Ok(planned) => {
                                module_plans.insert(name.clone(), Arc::new(planned));
                            }
                            Err(err) => {
                                let message = self.scrub(&err.message());
                                tracing::error!(buckle.module = %name, error = %message, "planning the deferred module failed");
                                started_modules.insert(name.clone());
                                done.insert(name.clone());
                                results.push(ModuleOutcome {
                                    module: name.clone(),
                                    status: ModuleStatus::Failed,
                                    changes: 0,
                                    duration_ms: 0,
                                    error: Some(format!("planning failed: {message}")),
                                });
                                failed.push(name.clone());
                                halted = true;
                                break;
                            }
                        }
                    }
                    let Some(module_plan) = module_plans.get(name).cloned() else {
                        continue;
                    };
                    started_modules.insert(name.clone());
                    let rev = revision.clone();
                    let contexts = Arc::new(planner.contexts(facts));
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
            if run.failed {
                failed.push(name.clone());
                halted = true;
            } else if let Some(providers) = provides.get(&name).filter(|p| !p.is_empty()) {
                let cx = planner.facts_cx();
                crate::facts::collect(&cx, facts, Some(providers), true).await;
                self.emit(ApplyEvent::FactsRefreshed {
                    providers: providers.clone(),
                })
                .await;
            }
            results.push(run.outcome);
            self.with_state(|s| s.facts = facts.clone());
            if let Err(e) = self.save_state() {
                errors.push(format!("state could not be saved: {}", e.message()));
            }
        }

        // Module-scoped rollback of whatever failed, after in-flight modules drained.
        let mut rolled_back = Vec::new();
        for name in &failed {
            match self.rollback(name, facts).await {
                Ok(Some(run)) => {
                    health.extend(run.health);
                    if run.outcome.status == ModuleStatus::RolledBack {
                        rolled_back.push(name.clone());
                    } else {
                        errors.push(format!(
                            "rolling back '{name}' failed: {}",
                            run.outcome.error.clone().unwrap_or_default()
                        ));
                    }
                    if let Some(existing) = results.iter_mut().find(|r| &r.module == name) {
                        let original_error = existing.error.take();
                        existing.status = run.outcome.status;
                        existing.error = match (original_error, run.outcome.error) {
                            (Some(o), Some(r)) => Some(format!("{o}; rollback: {r}")),
                            (o, r) => o.or(r),
                        };
                    }
                }
                Ok(None) => {}
                Err(err) => errors.push(format!(
                    "rolling back '{name}' failed: {}",
                    self.scrub(&err.message())
                )),
            }
            if let Err(e) = self.save_state() {
                errors.push(format!("state could not be saved: {}", e.message()));
            }
        }

        let not_started: Vec<String> = order
            .iter()
            .filter(|n| !done.contains(*n))
            .cloned()
            .collect();
        for name in &not_started {
            results.push(ModuleOutcome {
                module: name.clone(),
                status: ModuleStatus::NotStarted,
                changes: 0,
                duration_ms: 0,
                error: None,
            });
        }

        let mut removed = 0usize;
        if !halted && !self.inputs.removals && !rich.plan.removals.is_empty() {
            tracing::info!(
                skipped = rich.plan.removals.len(),
                "removals are disabled for this run; the planned removals were left in place"
            );
        }
        if !halted && self.inputs.removals {
            let contexts = planner.contexts(facts);
            for (index, removal) in rich.plan.removals.iter().enumerate() {
                let rr = &rich.removals[index];
                let span = tracing::info_span!("removal.run", buckle.resource = %removal.resource, buckle.policy = ?removal.policy);
                let outcome: Result<String, String> = async {
                    match (removal.policy, rr.task) {
                        (RemovalPolicy::Leave, _) => Ok("released: left in place (policy leave)".to_owned()),
                        (RemovalPolicy::Cleanup, None) => Ok(format!(
                            "forgotten: the task kind '{}' is no longer available, nothing was cleaned up",
                            removal.resource.kind
                        )),
                        (RemovalPolicy::Cleanup, Some(task)) => {
                            let cx = planner.removal_cx(&contexts, &rr.record);
                            let _guards = self.locks.acquire(&task.locks().iter().map(|l| l.to_string()).collect::<Vec<_>>()).await;
                            task.remove(&cx, &rr.record)
                                .await
                                .map(|a| self.scrub(&a.summary))
                                .map_err(|e| self.scrub(&e.message()))
                        }
                    }
                }
                .instrument(span)
                .await;
                match outcome {
                    Ok(summary) => {
                        self.with_state(|s| {
                            s.forget(&removal.resource);
                        });
                        removed += 1;
                        self.emit(ApplyEvent::RemovalRun {
                            resource: removal.resource.to_string(),
                            summary,
                        })
                        .await;
                    }
                    Err(message) => {
                        errors.push(format!("removing {} failed: {message}", removal.resource));
                        self.emit(ApplyEvent::RemovalRun {
                            resource: removal.resource.to_string(),
                            summary: format!("failed: {message}"),
                        })
                        .await;
                    }
                }
            }
        }

        let succeeded = !halted && errors.is_empty() && plan_errors.is_empty();
        let failed_module = failed.first().or(plan_errors.first()).cloned();
        let error = {
            let mut parts: Vec<String> = results
                .iter()
                .filter(|r| failed.contains(&r.module) || plan_errors.contains(&r.module))
                .filter_map(|r| {
                    r.error
                        .clone()
                        .map(|e| format!("module '{}': {e}", r.module))
                })
                .collect();
            parts.extend(errors.iter().cloned());
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
        } else {
            RunOutcome::Failed
        };
        let finished_at = self.inputs.system.now();
        self.with_state(|s| {
            s.facts = facts.clone();
            if succeeded {
                s.applied_revision = Some(revision.clone());
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
        self.save_state()?;

        let deferred_names: BTreeSet<&String> = deferred.iter().collect();
        tracing::info!(
            succeeded,
            changes = total_changes,
            removals = removed,
            deferred = deferred_names.len(),
            "apply finished"
        );
        Ok(ApplyOutcome {
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
        })
    }
}
