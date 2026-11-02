//! Test doubles for the agent: a scripted [`FakeRunner`], a recording
//! [`FakeUpdater`] and plan/health builders. Enabled for this crate's tests
//! and, with the `testing` feature, for other crates'.

use crate::runner::{RunContext, Runner};
use crate::update::{UpdateOutcome, Updater};
use async_trait::async_trait;
use buckle_core::engine::{ApplyEvent, ApplyOutcome, ModuleOutcome, ModuleStatus, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, ModulePlan, Plan, Planned, ResourceKey, RevisionId, Step, TaskRef};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

/// A plan for `revision` with `changes` changed steps in module `m`.
pub fn plan_with_changes(revision: &RevisionId, changes: usize) -> Plan {
    let steps = (0..changes.max(1))
        .map(|i| Step {
            task: TaskRef {
                module: "m".into(),
                id: format!("t{i}"),
                kind: "file.copy".into(),
                name: None,
            },
            resource: ResourceKey::new("m", "file.copy", format!("/etc/f{i}")),
            planned: if changes == 0 {
                Planned::NoOp
            } else {
                Planned::change(Diff::Opaque {
                    summary: format!("write /etc/f{i}"),
                })
            },
            locks: vec![],
            notifies: vec![],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
        })
        .collect();
    Plan::new(
        revision.clone(),
        "fake-host",
        vec![ModulePlan {
            module: "m".into(),
            needs: vec![],
            steps,
            handlers: vec![],
            provides_facts: vec![],
        }],
        vec![],
        vec![],
    )
}

/// A successful outcome for `plan`.
pub fn succeeded(plan: &Plan) -> ApplyOutcome {
    ApplyOutcome {
        plan_digest: plan.digest.clone(),
        succeeded: true,
        changes: plan.changes(),
        removals: plan.removals.len(),
        duration_ms: 12,
        module_results: plan
            .modules
            .iter()
            .map(|m| ModuleOutcome {
                module: m.module.clone(),
                status: ModuleStatus::Applied,
                changes: m.changes(),
                duration_ms: 12,
                error: None,
            })
            .collect(),
        failed_module: None,
        rolled_back_modules: vec![],
        not_started: vec![],
        health: None,
        error: None,
    }
}

/// A health report with one required check, passing or failing.
pub fn health(scope: HealthScope, passing: bool) -> HealthReport {
    HealthReport {
        scope,
        results: vec![HealthResult {
            module: "m".into(),
            check: "api".into(),
            kind: "http".into(),
            required: true,
            status: if passing {
                HealthStatus::Pass
            } else {
                HealthStatus::fail("connection refused")
            },
            at: chrono::Utc::now(),
            duration_ms: 1,
        }],
    }
}

/// A call the agent made on the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `facts`.
    Facts,
    /// `plan`.
    Plan {
        /// The revision.
        revision: RevisionId,
        /// The selector.
        select: Option<String>,
        /// Waived checks.
        waived_checks: Vec<String>,
        /// Force.
        force: bool,
    },
    /// `apply`.
    Apply {
        /// The revision.
        revision: RevisionId,
        /// The plan's digest.
        plan_digest: String,
        /// Waived checks.
        waived_checks: Vec<String>,
        /// Force.
        force: bool,
    },
    /// `health`.
    Health {
        /// The revision.
        revision: RevisionId,
        /// The scope.
        scope: HealthScope,
        /// The modules.
        modules: Option<Vec<String>>,
    },
}

type HealthFn = Box<dyn Fn(&RevisionId, HealthScope) -> HealthReport + Send + Sync>;
type OutcomeFn = Box<dyn Fn(&Plan) -> ApplyOutcome + Send + Sync>;
type ApplyErrorFn = Box<dyn Fn(&Plan) -> Option<buckle_core::Error> + Send + Sync>;

/// A scripted runner that records every call.
pub struct FakeRunner {
    plans: Mutex<HashMap<RevisionId, VecDeque<Plan>>>,
    outcome: Mutex<OutcomeFn>,
    apply_error: Mutex<Option<ApplyErrorFn>>,
    health: Mutex<HealthFn>,
    events_per_apply: Mutex<Vec<ApplyEvent>>,
    facts: Mutex<Facts>,
    plan_facts: Mutex<Option<Facts>>,
    calls: Mutex<Vec<Call>>,
    changed: Notify,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            plans: Mutex::new(HashMap::new()),
            outcome: Mutex::new(Box::new(succeeded)),
            apply_error: Mutex::new(None),
            health: Mutex::new(Box::new(|_, scope| health(scope, true))),
            events_per_apply: Mutex::new(Vec::new()),
            facts: Mutex::new(Facts::new()),
            plan_facts: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            changed: Notify::new(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeRunner {
    /// A runner that plans nothing, applies successfully and is healthy.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Plans `revision` as `plans` in turn; the last one repeats.
    pub fn with_plans(&self, revision: &RevisionId, plans: Vec<Plan>) -> &Self {
        lock(&self.plans).insert(revision.clone(), plans.into());
        self
    }

    /// Computes apply outcomes with `f`.
    pub fn with_outcome(&self, f: impl Fn(&Plan) -> ApplyOutcome + Send + Sync + 'static) -> &Self {
        *lock(&self.outcome) = Box::new(f);
        self
    }

    /// Makes `apply` fail with the error `f` returns (when it returns one),
    /// after recording the call.
    pub fn with_apply_error(
        &self,
        f: impl Fn(&Plan) -> Option<buckle_core::Error> + Send + Sync + 'static,
    ) -> &Self {
        *lock(&self.apply_error) = Some(Box::new(f));
        self
    }

    /// Computes health reports with `f`.
    pub fn with_health(
        &self,
        f: impl Fn(&RevisionId, HealthScope) -> HealthReport + Send + Sync + 'static,
    ) -> &Self {
        *lock(&self.health) = Box::new(f);
        self
    }

    /// Emits these events during every apply.
    pub fn with_events(&self, events: Vec<ApplyEvent>) -> &Self {
        *lock(&self.events_per_apply) = events;
        self
    }

    /// Returns these facts from `facts`.
    pub fn with_facts(&self, facts: Facts) -> &Self {
        *lock(&self.facts) = facts;
        self
    }

    /// Returns these facts from `plan`, as the engine does when a provider's
    /// TTL expired during planning (otherwise `plan` hands back the context's).
    pub fn with_plan_facts(&self, facts: Facts) -> &Self {
        *lock(&self.plan_facts) = Some(facts);
        self
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    /// The applies so far.
    pub fn applies(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Apply { .. }))
            .collect()
    }

    /// Waits until `pred` holds over the calls.
    pub async fn wait_for(&self, pred: impl Fn(&[Call]) -> bool) {
        loop {
            let notified = self.changed.notified();
            if pred(&self.calls()) {
                return;
            }
            notified.await;
        }
    }

    fn record(&self, call: Call) {
        lock(&self.calls).push(call);
        self.changed.notify_waiters();
    }

    fn plan_for(&self, revision: &RevisionId) -> Plan {
        let mut plans = lock(&self.plans);
        match plans.get_mut(revision) {
            Some(queue) if queue.len() > 1 => queue
                .pop_front()
                .unwrap_or_else(|| plan_with_changes(revision, 0)),
            Some(queue) => queue
                .front()
                .cloned()
                .unwrap_or_else(|| plan_with_changes(revision, 0)),
            None => plan_with_changes(revision, 0),
        }
    }
}

#[async_trait]
impl Runner for FakeRunner {
    async fn facts(&self, _previous: &Facts) -> buckle_core::Result<Facts> {
        self.record(Call::Facts);
        Ok(lock(&self.facts).clone())
    }

    async fn plan(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
    ) -> buckle_core::Result<(Plan, Facts)> {
        self.record(Call::Plan {
            revision: revision.id.clone(),
            select: ctx.select.clone(),
            waived_checks: ctx.waived_checks.clone(),
            force: ctx.force,
        });
        let facts = lock(&self.plan_facts)
            .clone()
            .unwrap_or_else(|| ctx.facts.clone());
        Ok((self.plan_for(&revision.id), facts))
    }

    async fn apply(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> buckle_core::Result<ApplyOutcome> {
        self.record(Call::Apply {
            revision: revision.id.clone(),
            plan_digest: plan.digest.to_string(),
            waived_checks: ctx.waived_checks.clone(),
            force: ctx.force,
        });
        if let Some(f) = lock(&self.apply_error).as_ref()
            && let Some(err) = f(plan)
        {
            return Err(err);
        }
        let scripted = lock(&self.events_per_apply).clone();
        for event in scripted {
            let _ = events.send(event).await;
        }
        let outcome = (lock(&self.outcome))(plan);
        Ok(outcome)
    }

    async fn health(
        &self,
        revision: Arc<Revision>,
        _ctx: &RunContext,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> buckle_core::Result<HealthReport> {
        self.record(Call::Health {
            revision: revision.id.clone(),
            scope,
            modules: modules.map(<[String]>::to_vec),
        });
        let report = (lock(&self.health))(&revision.id, scope);
        Ok(report)
    }
}

/// An updater that records requests and reports success.
#[derive(Default)]
pub struct FakeUpdater {
    /// The versions requested.
    pub requested: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl Updater for FakeUpdater {
    async fn update(&self, version: Option<&str>) -> buckle_core::Result<UpdateOutcome> {
        lock(&self.requested).push(version.map(str::to_owned));
        Ok(UpdateOutcome::Updated {
            version: version.unwrap_or("latest").to_owned(),
        })
    }
}
