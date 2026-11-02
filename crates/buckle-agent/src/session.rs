//! The session loop: connect, say Hello, follow instructions, report, and
//! reconnect with backoff; plus the drift pass, heartbeats, facts refresh and
//! the local rollback watchdog.
//!
//! Operations (assign, plan, apply, rollback, health, drift, facts, upgrade,
//! watchdog checks) run one at a time on a worker task, in the order they
//! were queued, so the session keeps heartbeating while an apply runs and
//! `busy` in the heartbeat reflects the worker. Operations queued by a
//! session that has since ended are skipped (the server re-drives the host
//! after it reconnects), except an apply that already started, which always
//! runs to completion.

use crate::backoff::Backoff;
use crate::coalesce;
use crate::config::AgentConfig;
use crate::runner::{RunContext, Runner};
use crate::sink::ReportSink;
use crate::store::{AgentState, Store};
use crate::transport::grpc::set_parent_from_traceparent;
use crate::transport::{
    Hello, Instruction, LocalRollbackRecord, Report, RevisionSource, Session, Transport,
    Welcome, WireSecret,
};
use crate::update::{UpdateOutcome, Updater};
use crate::watchdog::{Verdict, Watchdog};
use buckle_core::engine::{ApplyOutcome, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use buckle_core::plan::{Plan, RevisionId};
use buckle_core::secrets::{InstallKey, Secret, SecretStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Tunables of the session loop. The defaults are the design's values; tests
/// shorten some of them.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// Reported in Hello.
    pub agent_version: String,
    /// Liveness heartbeat interval (30 s).
    pub heartbeat: Duration,
    /// How often facts are re-collected (10 min).
    pub facts_interval: Duration,
    /// Reconnect backoff bounds (1 s to 5 min).
    pub backoff_min: Duration,
    /// Reconnect backoff cap.
    pub backoff_max: Duration,
    /// Whether the backoff is jittered.
    pub backoff_jitter: bool,
    /// Minimum spacing between ApplyEvent messages (100 ms, i.e. 10/s).
    pub event_spacing: Duration,
    /// How long shutdown waits for a running operation.
    pub shutdown_grace: Duration,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            heartbeat: Duration::from_secs(30),
            facts_interval: Duration::from_secs(600),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            backoff_jitter: true,
            event_spacing: coalesce::EVENT_SPACING,
            shutdown_grace: Duration::from_secs(60),
        }
    }
}

/// One unit of work for the worker.
#[derive(Debug, Clone)]
enum Op {
    Assign {
        revision: RevisionId,
        stable: bool,
        secrets: Vec<WireSecret>,
        tree_digest: Option<String>,
        /// From the server's Assign (the desired revision), rather than the
        /// agent caching the stable revision named in Welcome.
        desired: bool,
    },
    Plan {
        revision: RevisionId,
        select: Option<String>,
    },
    Apply {
        revision: RevisionId,
        plan_digest: Option<String>,
        waived_checks: Vec<String>,
        force: bool,
        traceparent: Option<String>,
    },
    Rollback {
        revision: RevisionId,
        reason: String,
        traceparent: Option<String>,
    },
    Health {
        request_id: String,
        scope: HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        traceparent: Option<String>,
    },
    Drift,
    Facts {
        force_report: bool,
    },
    Upgrade {
        version: String,
    },
    WatchdogCheck,
}

struct Queued {
    op: Op,
    /// The session that queued it; `None` for agent-initiated work.
    generation: Option<u64>,
}

#[derive(Clone)]
struct Queue {
    tx: mpsc::UnboundedSender<Queued>,
    len: Arc<AtomicUsize>,
}

impl Queue {
    fn push(&self, op: Op, generation: Option<u64>) {
        self.len.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(Queued { op, generation }).is_err() {
            self.len.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn is_empty(&self) -> bool {
        self.len.load(Ordering::SeqCst) == 0
    }
}

#[derive(Default)]
struct Data {
    revisions: HashMap<RevisionId, Arc<Revision>>,
    secrets: HashMap<RevisionId, Arc<SecretStore>>,
    facts: Facts,
    agent: AgentState,
    applied: Option<RevisionId>,
    stable: Option<RevisionId>,
    assigned: Option<RevisionId>,
}

struct Shared {
    config: AgentConfig,
    options: AgentOptions,
    host_id: String,
    store: Store,
    transport: Arc<dyn Transport>,
    runner: Arc<dyn Runner>,
    updater: Arc<dyn Updater>,
    install_key: InstallKey,
    sink: ReportSink,
    busy: AtomicBool,
    connected: AtomicBool,
    generation: AtomicU64,
    session_cancel: Mutex<CancellationToken>,
    stop: CancellationToken,
    data: tokio::sync::Mutex<Data>,
    watchdog: Mutex<Option<Watchdog>>,
    watchdog_wake: Notify,
    watchdog_queued: AtomicBool,
}

enum SessionEnd {
    Closed { welcomed: bool },
    Rejected { reason: String, retry: bool },
    Shutdown,
}

/// The agent: a session loop over a [`Transport`], doing its work through a
/// [`Runner`].
pub struct Agent {
    shared: Arc<Shared>,
}

fn labels_json(labels: &indexmap::IndexMap<String, String>) -> buckle_core::selectors::Labels {
    labels
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

fn failed_outcome(plan: &Plan, error: &buckle_core::Error, started: Instant) -> ApplyOutcome {
    ApplyOutcome {
        plan_digest: plan.digest.clone(),
        succeeded: false,
        changes: 0,
        removals: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        module_results: Vec::new(),
        failed_module: None,
        rolled_back_modules: Vec::new(),
        not_started: plan.modules.iter().map(|m| m.module.clone()).collect(),
        health: None,
        error: Some(error.description()),
    }
}

fn health_error_report(scope: HealthScope, error: &buckle_core::Error) -> HealthReport {
    HealthReport {
        scope,
        results: vec![HealthResult {
            module: "agent".into(),
            check: "health".into(),
            kind: "agent".into(),
            required: true,
            status: HealthStatus::unknown(format!("the checks could not run: {}", error.description())),
            at: chrono::Utc::now(),
            duration_ms: 0,
        }],
    }
}

/// How often bake checks repeat: the shortest declared check interval, capped at 30 s.
fn health_interval(revision: &Revision) -> Duration {
    revision
        .modules
        .iter()
        .flat_map(|m| m.config.health.iter().map(|h| h.interval))
        .min()
        .unwrap_or(Duration::from_secs(30))
        .clamp(Duration::from_secs(1), Duration::from_secs(30))
}

impl Agent {
    /// An agent for `config` over `transport`, running through `runner`.
    pub fn new(
        config: AgentConfig,
        transport: Arc<dyn Transport>,
        runner: Arc<dyn Runner>,
        updater: Arc<dyn Updater>,
        options: AgentOptions,
    ) -> buckle_core::Result<Self> {
        let store = Store::new(&config.state_dir);
        store.ensure()?;
        let install_key = store.install_key()?;
        let host_id = config.host_id.clone().unwrap_or_else(|| {
            use buckle_core::system::System;
            buckle_core::system::RealSystem::new().hostname()
        });
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                options,
                host_id,
                store,
                transport,
                runner,
                updater,
                install_key,
                sink: ReportSink::default(),
                busy: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                session_cancel: Mutex::new(CancellationToken::new()),
                stop: CancellationToken::new(),
                data: tokio::sync::Mutex::new(Data::default()),
                watchdog: Mutex::new(None),
                watchdog_wake: Notify::new(),
                watchdog_queued: AtomicBool::new(false),
            }),
        })
    }

    /// Runs until `shutdown` trips, an upgrade asks for a restart (exit code
    /// 0), or the server rejects the host without retry (an error).
    pub async fn run(self, shutdown: CancellationToken) -> buckle_core::Result<i32> {
        let shared = self.shared;
        shared.load().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let queue = Queue {
            tx,
            len: Arc::new(AtomicUsize::new(0)),
        };
        let worker = tokio::spawn(Shared::worker(shared.clone(), rx, queue.len.clone()));
        let ticker_cancel = CancellationToken::new();
        let ticker = tokio::spawn(Shared::watchdog_ticker(
            shared.clone(),
            queue.clone(),
            ticker_cancel.clone(),
        ));
        queue.push(Op::Facts { force_report: false }, None);

        let mut backoff = Backoff::new(shared.options.backoff_min, shared.options.backoff_max);
        if !shared.options.backoff_jitter {
            backoff = backoff.without_jitter();
        }
        let result = loop {
            if shutdown.is_cancelled() || shared.stop.is_cancelled() {
                break Ok(0);
            }
            let hello = shared.hello().await;
            tracing::debug!(?hello, "connecting");
            let connected = tokio::select! {
                result = shared.transport.connect(hello) => Some(result),
                _ = shutdown.cancelled() => None,
                _ = shared.stop.cancelled() => None,
            };
            match connected {
                None => break Ok(0),
                Some(Ok(session)) => {
                    let span = tracing::info_span!("agent.session", host = %shared.host_id);
                    match shared
                        .clone()
                        .drive(session, &queue, &shutdown)
                        .instrument(span)
                        .await
                    {
                        SessionEnd::Shutdown => break Ok(0),
                        SessionEnd::Rejected { reason, retry: false } => {
                            tracing::error!(%reason, "the server rejected this host; not retrying");
                            break Err(buckle_core::error::user(
                                format!("The server rejected this host: {reason}"),
                                &["Ask a Buckle admin to approve or re-enrol the host, then restart the agent."],
                            ));
                        }
                        SessionEnd::Rejected { reason, retry: true } => {
                            tracing::warn!(%reason, "the server rejected the session; retrying");
                        }
                        SessionEnd::Closed { welcomed } => {
                            if welcomed {
                                backoff.reset();
                            }
                        }
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err.description(), "could not connect to the server");
                }
            }
            let delay = backoff.next_delay();
            tracing::info!(delay = ?delay, "reconnecting after a delay");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.cancelled() => break Ok(0),
                _ = shared.stop.cancelled() => break Ok(0),
            }
        };

        ticker_cancel.cancel();
        let _ = ticker.await;
        drop(queue);
        if tokio::time::timeout(shared.options.shutdown_grace, worker)
            .await
            .is_err()
        {
            tracing::warn!("an operation was still running at shutdown");
        }
        result
    }
}

impl Shared {
    async fn load(&self) -> buckle_core::Result<()> {
        let state = self.store.load_state()?;
        let agent = self.store.load_agent_state();
        if let Some(persisted) = agent.watchdog.clone() {
            tracing::info!(from = %persisted.from.short(), to = %persisted.to.short(), "resuming the local rollback watchdog");
            if let Ok(mut guard) = self.watchdog.lock() {
                *guard = Some(Watchdog::resume(persisted));
            }
        }
        let mut data = self.data.lock().await;
        data.applied = state.applied_revision;
        data.stable = state.stable_revision;
        if !state.facts.tree.is_null() {
            data.facts = state.facts;
        }
        data.agent = agent;
        Ok(())
    }

    fn save_agent(&self, data: &Data) {
        if let Err(err) = self.store.save_agent_state(&data.agent) {
            tracing::error!(error = %err.description(), "could not persist the agent's session state");
        }
    }

    async fn hello(&self) -> Hello {
        let data = self.data.lock().await;
        let issued = self.store.read_token().unwrap_or_else(|err| {
            tracing::warn!(error = %err.description(), "could not read the per-host token");
            None
        });
        let (token, bootstrap) = match issued {
            Some(t) => (Some(t), false),
            None => (
                self.config.bootstrap_token.clone(),
                self.config.bootstrap_token.is_some(),
            ),
        };
        Hello {
            host_id: self.host_id.clone(),
            agent_version: self.options.agent_version.clone(),
            labels: self.config.labels.clone(),
            applied_revision: data.applied.clone(),
            last_plan_digest: data.agent.last_plan_digest.clone(),
            token,
            platform: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            bootstrap,
        }
    }

    async fn drive(
        self: Arc<Self>,
        mut session: Session,
        queue: &Queue,
        shutdown: &CancellationToken,
    ) -> SessionEnd {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.sink.attach(session.reports.clone());
        let session_cancel = shutdown.child_token();
        if let Ok(mut guard) = self.session_cancel.lock() {
            *guard = session_cancel.clone();
        }
        let mut welcomed = false;
        let start = Instant::now();
        let mut heartbeat = tokio::time::interval_at(start + self.options.heartbeat, self.options.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let drift_every = self.config.drift.interval.max(Duration::from_secs(1));
        let mut drift = tokio::time::interval_at(start + drift_every, drift_every);
        drift.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut facts = tokio::time::interval_at(start + self.options.facts_interval, self.options.facts_interval);
        facts.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let end = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break SessionEnd::Shutdown,
                _ = self.stop.cancelled() => break SessionEnd::Shutdown,
                instruction = session.instructions.recv() => match instruction {
                    None => break SessionEnd::Closed { welcomed },
                    Some(Instruction::Welcome(welcome)) => {
                        self.on_welcome(welcome, queue).await;
                        welcomed = true;
                    }
                    Some(Instruction::Rejected { reason, retry }) => {
                        break SessionEnd::Rejected { reason, retry };
                    }
                    Some(Instruction::Ping) => {}
                    Some(other) => {
                        if let Some(op) = op_for(other) {
                            queue.push(op, Some(generation));
                        }
                    }
                },
                _ = heartbeat.tick() => if welcomed {
                    let applied = self.data.lock().await.applied.clone();
                    self.sink.send(Report::Heartbeat {
                        applied_revision: applied,
                        busy: self.busy.load(Ordering::SeqCst),
                    }).await;
                },
                _ = drift.tick() => if welcomed
                    && !self.busy.load(Ordering::SeqCst)
                    && queue.is_empty()
                    && self.data.lock().await.applied.is_some()
                {
                    queue.push(Op::Drift, None);
                },
                _ = facts.tick() => queue.push(Op::Facts { force_report: false }, None),
            }
        };
        self.connected.store(false, Ordering::SeqCst);
        self.sink.detach();
        session_cancel.cancel();
        self.watchdog_wake.notify_one();
        end
    }

    async fn on_welcome(&self, welcome: Welcome, queue: &Queue) {
        tracing::info!(
            server = %welcome.server_version,
            session = %welcome.session_id,
            status = %welcome.status,
            "session established"
        );
        if let Some(token) = &welcome.issued_token {
            match self.store.write_token(token) {
                Ok(()) => tracing::info!("stored the per-host token issued by the server"),
                Err(err) => tracing::error!(error = %err.description(), "could not store the per-host token"),
            }
        }
        let pending = {
            let mut data = self.data.lock().await;
            data.agent.labels = welcome.labels.clone();
            data.agent.status = Some(welcome.status.clone()).filter(|s| !s.is_empty());
            if let Some(stable) = &welcome.stable_revision {
                data.stable = Some(stable.clone());
                if let Err(err) = self
                    .store
                    .update_state(|s| s.stable_revision = Some(stable.clone()))
                {
                    tracing::error!(error = %err.description(), "could not record the stable revision");
                }
                if !welcome.secrets.is_empty() {
                    self.cache_secrets(&mut data, stable, &welcome.secrets);
                }
                if !self.store.has_revision(stable) {
                    queue.push(
                        Op::Assign {
                            revision: stable.clone(),
                            stable: true,
                            secrets: Vec::new(),
                            tree_digest: None,
                            desired: false,
                        },
                        None,
                    );
                }
            }
            if let Ok(mut guard) = self.watchdog.lock()
                && let Some(w) = guard.take()
            {
                tracing::info!(revision = %w.state.from.short(), "the session came back; the local rollback watchdog is disarmed");
                data.agent.watchdog = None;
            }
            let pending = std::mem::take(&mut data.agent.pending_rollbacks);
            self.save_agent(&data);
            pending
        };
        self.connected.store(true, Ordering::SeqCst);

        let mut unsent = Vec::new();
        for record in pending {
            if !self.sink.send(Report::LocalRollback(record.clone())).await {
                unsent.push(record);
            }
        }
        if !unsent.is_empty() {
            let mut data = self.data.lock().await;
            data.agent.pending_rollbacks.extend(unsent);
            self.save_agent(&data);
        }
        queue.push(Op::Facts { force_report: true }, None);
    }

    async fn worker(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<Queued>,
        len: Arc<AtomicUsize>,
    ) {
        while let Some(queued) = rx.recv().await {
            len.fetch_sub(1, Ordering::SeqCst);
            if let Some(generation) = queued.generation
                && generation != self.generation.load(Ordering::SeqCst)
            {
                tracing::debug!(op = ?queued.op, "skipping an operation queued by a session that has ended");
                continue;
            }
            self.busy.store(true, Ordering::SeqCst);
            self.execute(queued.op).await;
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    async fn execute(&self, op: Op) {
        match op {
            Op::Assign {
                revision,
                stable,
                secrets,
                tree_digest,
                desired,
            } => {
                self.assign(revision, stable, secrets, tree_digest, desired)
                    .await
            }
            Op::Plan { revision, select } => {
                let span = tracing::info_span!("agent.plan", revision = %revision.short());
                self.plan(revision, select).instrument(span).await
            }
            Op::Apply {
                revision,
                plan_digest,
                waived_checks,
                force,
                traceparent,
            } => {
                let span = tracing::info_span!("agent.apply", revision = %revision.short(), force);
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.apply(revision, plan_digest, waived_checks, force)
                    .instrument(span)
                    .await
            }
            Op::Rollback {
                revision,
                reason,
                traceparent,
            } => {
                let span = tracing::info_span!("agent.apply", revision = %revision.short(), rollback = true);
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.rollback(revision, reason).instrument(span).await
            }
            Op::Health {
                request_id,
                scope,
                modules,
                until,
                traceparent,
            } => {
                let span = tracing::info_span!("agent.health", scope = ?scope, request = %request_id);
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.health(request_id, scope, modules, until)
                    .instrument(span)
                    .await
            }
            Op::Drift => {
                let span = tracing::info_span!("agent.drift");
                self.drift().instrument(span).await
            }
            Op::Facts { force_report } => self.facts(force_report).await,
            Op::Upgrade { version } => self.upgrade(version).await,
            Op::WatchdogCheck => {
                let span = tracing::info_span!("agent.watchdog");
                self.watchdog_check().instrument(span).await
            }
        }
    }

    // ------------------------------------------------------------ revisions and secrets

    fn build_secrets(&self, wire: &[WireSecret]) -> SecretStore {
        let mut store = SecretStore::new();
        for secret in wire {
            match String::from_utf8(secret.value.clone()) {
                Ok(value) => store.insert(Secret::new(secret.name.clone(), value, &self.install_key)),
                Err(_) => tracing::warn!(secret = %secret.name, "ignoring a secret whose value is not UTF-8"),
            }
        }
        store
    }

    fn cache_secrets(&self, data: &mut Data, revision: &RevisionId, wire: &[WireSecret]) {
        let mut merged: Vec<WireSecret> = self
            .store
            .read_secrets(revision)
            .ok()
            .flatten()
            .unwrap_or_default();
        for secret in wire {
            merged.retain(|s| s.name != secret.name);
            merged.push(secret.clone());
        }
        if let Err(err) = self.store.write_secrets(revision, &merged) {
            tracing::error!(error = %err.description(), "could not cache secrets");
        }
        data.secrets
            .insert(revision.clone(), Arc::new(self.build_secrets(&merged)));
    }

    async fn secrets_for(&self, revision: &RevisionId) -> Arc<SecretStore> {
        let mut data = self.data.lock().await;
        if let Some(s) = data.secrets.get(revision) {
            return s.clone();
        }
        let store = match self.store.read_secrets(revision) {
            Ok(Some(wire)) => Arc::new(self.build_secrets(&wire)),
            Ok(None) => Arc::new(SecretStore::new()),
            Err(err) => {
                tracing::error!(error = %err.description(), "could not read cached secrets");
                Arc::new(SecretStore::new())
            }
        };
        data.secrets.insert(revision.clone(), store.clone());
        store
    }

    async fn ensure_revision(
        &self,
        revision: &RevisionId,
        tree_digest: Option<&str>,
    ) -> buckle_core::Result<Arc<Revision>> {
        if let Some(r) = self.data.lock().await.revisions.get(revision) {
            return Ok(r.clone());
        }
        let dir = if self.store.has_revision(revision) {
            self.store.revision_dir(revision)?
        } else {
            match self.transport.fetch_revision(revision, tree_digest).await? {
                RevisionSource::Directory(dir) => dir,
                RevisionSource::Tarball(bytes) => {
                    let dest = self.store.revision_dir(revision)?;
                    crate::bundle::extract_atomic(&bytes, &dest)?;
                    tracing::info!(revision = %revision.short(), bytes = bytes.len(), "fetched and cached a revision");
                    dest
                }
            }
        };
        let loaded = Arc::new(buckle_core::engine::loader::load_dir(&dir, revision.clone())?);
        let mut data = self.data.lock().await;
        let keep: Vec<RevisionId> = [data.applied.clone(), data.stable.clone(), data.assigned.clone()]
            .into_iter()
            .flatten()
            .collect();
        data.revisions
            .retain(|id, _| keep.contains(id));
        data.revisions.insert(revision.clone(), loaded.clone());
        Ok(loaded)
    }

    async fn context(
        &self,
        revision: &RevisionId,
        select: Option<String>,
        waived_checks: Vec<String>,
        force: bool,
    ) -> RunContext {
        let secrets = self.secrets_for(revision).await;
        let data = self.data.lock().await;
        RunContext {
            host: buckle_core::selectors::HostIdentity {
                id: self.host_id.clone(),
                labels: labels_json(&self.config.labels),
            },
            server_labels: labels_json(&data.agent.labels),
            secrets,
            facts: data.facts.clone(),
            select,
            waived_checks,
            force,
        }
    }

    async fn remember_facts(&self, facts: Facts) {
        self.data.lock().await.facts = facts;
    }

    async fn remember_plan(&self, plan: &Plan) {
        let mut data = self.data.lock().await;
        data.agent.last_plan_digest = Some(plan.digest.to_string());
        self.save_agent(&data);
    }

    // ------------------------------------------------------------ operations

    async fn assign(
        &self,
        revision: RevisionId,
        stable: bool,
        secrets: Vec<WireSecret>,
        tree_digest: Option<String>,
        desired: bool,
    ) {
        if !secrets.is_empty() {
            let mut data = self.data.lock().await;
            self.cache_secrets(&mut data, &revision, &secrets);
        }
        {
            let mut data = self.data.lock().await;
            if stable {
                data.stable = Some(revision.clone());
            }
            if desired {
                data.assigned = Some(revision.clone());
            }
        }
        if stable
            && let Err(err) = self
                .store
                .update_state(|s| s.stable_revision = Some(revision.clone()))
        {
            tracing::error!(error = %err.description(), "could not record the stable revision");
        }
        match self.ensure_revision(&revision, tree_digest.as_deref()).await {
            Ok(_) => {
                tracing::info!(revision = %revision.short(), stable, "revision assigned");
                let data = self.data.lock().await;
                let keep: Vec<&RevisionId> = [&data.applied, &data.stable, &data.assigned]
                    .into_iter()
                    .flatten()
                    .collect();
                self.store.prune_revisions(&keep);
            }
            Err(err) => {
                tracing::error!(revision = %revision.short(), error = %err.description(), "could not fetch the assigned revision");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                    })
                    .await;
            }
        }
    }

    async fn plan(&self, revision: RevisionId, select: Option<String>) {
        let result = async {
            let rev = self.ensure_revision(&revision, None).await?;
            let ctx = self.context(&revision, select, Vec::new(), false).await;
            self.runner.plan(rev, &ctx).await
        }
        .await;
        match result {
            Ok((plan, facts)) => {
                self.remember_facts(facts).await;
                self.remember_plan(&plan).await;
                tracing::info!(digest = %plan.digest.short(), changes = plan.changes(), "planned");
                self.sink.send(Report::Plan(plan)).await;
            }
            Err(err) => {
                tracing::error!(error = %err.description(), "planning failed");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                    })
                    .await;
            }
        }
    }

    /// Applies `plan` and records the outcome. `report` sends ApplyEvents and
    /// the ApplyResult to the current session.
    async fn run_apply(
        &self,
        rev: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        report: bool,
    ) -> ApplyOutcome {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel(256);
        let sink = if report {
            self.sink.clone()
        } else {
            ReportSink::default()
        };
        let forwarder = tokio::spawn(coalesce::forward(
            rx,
            sink,
            rev.id.clone(),
            plan.digest.to_string(),
            self.options.event_spacing,
        ));
        let result = self.runner.apply(rev.clone(), ctx, plan, tx).await;
        let _ = forwarder.await;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::error!(error = %err.description(), "the apply failed");
                failed_outcome(plan, &err, started)
            }
        };
        let applied = {
            let mut data = self.data.lock().await;
            if outcome.succeeded {
                data.applied = Some(rev.id.clone());
                if let Err(err) = self
                    .store
                    .update_state(|s| s.applied_revision = Some(rev.id.clone()))
                {
                    tracing::error!(error = %err.description(), "could not record the applied revision");
                }
            }
            data.agent.last_plan_digest = Some(plan.digest.to_string());
            self.save_agent(&data);
            data.applied.clone()
        };
        if report {
            self.sink
                .send(Report::ApplyResult {
                    revision: rev.id.clone(),
                    outcome: outcome.clone(),
                    applied_revision: applied,
                })
                .await;
        }
        outcome
    }

    async fn apply(
        &self,
        revision: RevisionId,
        plan_digest: Option<String>,
        waived_checks: Vec<String>,
        force: bool,
    ) {
        let connected_before = self.connected.load(Ordering::SeqCst);
        let prepared = async {
            let rev = self.ensure_revision(&revision, None).await?;
            let ctx = self
                .context(&revision, None, waived_checks.clone(), force)
                .await;
            let (plan, facts) = self.runner.plan(rev.clone(), &ctx).await?;
            Ok::<_, buckle_core::Error>((rev, ctx, plan, facts))
        }
        .await;
        let (rev, ctx, plan, facts) = match prepared {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(error = %err.description(), "could not plan the revision to apply");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                    })
                    .await;
                return;
            }
        };
        self.remember_facts(facts).await;
        // The approved digest binds the apply; `force` only overrides the
        // server's baseline hold, never this check.
        if let Some(approved) = &plan_digest
            && !crate::bundle::digests_match(approved, plan.digest.as_str())
        {
            tracing::warn!(approved = %approved, fresh = %plan.digest, "refusing a stale plan");
            self.remember_plan(&plan).await;
            self.sink
                .send(Report::StalePlan {
                    plan,
                    approved_digest: approved.clone(),
                })
                .await;
            return;
        }
        let outcome = self.run_apply(rev.clone(), &ctx, &plan, true).await;
        tracing::info!(succeeded = outcome.succeeded, changes = outcome.changes, "apply finished");
        self.maybe_arm_watchdog(&rev, connected_before, waived_checks)
            .await;
    }

    async fn maybe_arm_watchdog(
        &self,
        rev: &Revision,
        connected_before: bool,
        waived_checks: Vec<String>,
    ) {
        let policy = rev.fleet.rollout.local_rollback.clone();
        if !policy.enabled {
            return;
        }
        if !connected_before {
            tracing::debug!("not arming the local rollback watchdog: there was no session before the apply");
            return;
        }
        let mut data = self.data.lock().await;
        let Some(stable) = data.stable.clone() else {
            tracing::debug!("not arming the local rollback watchdog: no stable revision is known");
            return;
        };
        if stable == rev.id {
            return;
        }
        if !self.store.has_revision(&stable) {
            tracing::warn!(stable = %stable.short(), "not arming the local rollback watchdog: the stable revision is not cached");
            return;
        }
        let watchdog = Watchdog::arm(
            rev.id.clone(),
            stable,
            &policy,
            rev.fleet.rollout.settle,
            waived_checks,
        );
        data.agent.watchdog = Some(watchdog.state.clone());
        self.save_agent(&data);
        if let Ok(mut guard) = self.watchdog.lock() {
            *guard = Some(watchdog);
        }
        self.watchdog_wake.notify_one();
    }

    async fn rollback(&self, revision: RevisionId, reason: String) {
        tracing::warn!(revision = %revision.short(), %reason, "rolling back on the server's instruction");
        let prepared = async {
            let rev = self.ensure_revision(&revision, None).await?;
            let ctx = self.context(&revision, None, Vec::new(), true).await;
            let (plan, facts) = self.runner.plan(rev.clone(), &ctx).await?;
            Ok::<_, buckle_core::Error>((rev, ctx, plan, facts))
        }
        .await;
        match prepared {
            Ok((rev, ctx, plan, facts)) => {
                self.remember_facts(facts).await;
                self.remember_plan(&plan).await;
                self.run_apply(rev, &ctx, &plan, true).await;
                // A server-driven rollback supersedes any local watchdog.
                self.disarm_watchdog().await;
            }
            Err(err) => {
                tracing::error!(error = %err.description(), "could not plan the rollback");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                    })
                    .await;
            }
        }
    }

    async fn health(
        &self,
        request_id: String,
        scope: HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let applied = self.data.lock().await.applied.clone();
        let Some(revision) = applied else {
            self.sink
                .send(Report::Health {
                    request_id: Some(request_id),
                    revision: None,
                    report: HealthReport {
                        scope,
                        results: Vec::new(),
                    },
                })
                .await;
            return;
        };
        let rev = match self.ensure_revision(&revision, None).await {
            Ok(rev) => rev,
            Err(err) => {
                self.sink
                    .send(Report::Health {
                        request_id: Some(request_id),
                        revision: Some(revision),
                        report: health_error_report(scope, &err),
                    })
                    .await;
                return;
            }
        };
        let ctx = self.context(&revision, None, Vec::new(), false).await;
        let modules = (!modules.is_empty()).then_some(modules);
        let deadline = until.map(|until| {
            Instant::now() + (until - chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO)
        });
        let interval = health_interval(&rev);
        let cancel = self
            .session_cancel
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        loop {
            let report = match self
                .runner
                .health(rev.clone(), &ctx, scope, modules.as_deref())
                .await
            {
                Ok(report) => report,
                Err(err) => health_error_report(scope, &err),
            };
            self.sink
                .send(Report::Health {
                    request_id: Some(request_id.clone()),
                    revision: Some(revision.clone()),
                    report,
                })
                .await;
            let Some(deadline) = deadline else { break };
            if Instant::now() + interval >= deadline {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = cancel.cancelled() => break,
            }
        }
    }

    async fn drift(&self) {
        let (applied, paused) = {
            let data = self.data.lock().await;
            (
                data.applied.clone(),
                data.agent.status.as_deref() == Some("paused"),
            )
        };
        let Some(revision) = applied else { return };
        let rev = match self.ensure_revision(&revision, None).await {
            Ok(rev) => rev,
            Err(err) => {
                tracing::warn!(error = %err.description(), "could not load the applied revision for the drift pass");
                return;
            }
        };
        let ctx = self.context(&revision, None, Vec::new(), false).await;
        match self.runner.plan(rev.clone(), &ctx).await {
            Ok((plan, facts)) => {
                self.remember_facts(facts).await;
                let drifted = !plan.is_empty();
                let mut corrected = false;
                if drifted {
                    tracing::warn!(changes = plan.changes(), removals = plan.removals.len(), "drift detected");
                    if self.config.drift.correct && !paused {
                        corrected = self.run_apply(rev.clone(), &ctx, &plan, false).await.succeeded;
                    }
                }
                self.sink
                    .send(Report::Drift {
                        revision: revision.clone(),
                        plan: drifted.then_some(plan),
                        corrected,
                    })
                    .await;
            }
            Err(err) => {
                tracing::warn!(error = %err.description(), "the drift pass could not plan");
            }
        }
        let report = match self
            .runner
            .health(rev, &ctx, HealthScope::Drift, None)
            .await
        {
            Ok(report) => report,
            Err(err) => health_error_report(HealthScope::Drift, &err),
        };
        self.sink
            .send(Report::Health {
                request_id: None,
                revision: Some(revision),
                report,
            })
            .await;
    }

    async fn facts(&self, force_report: bool) {
        let previous = self.data.lock().await.facts.clone();
        match self.runner.facts(&previous).await {
            Ok(facts) => {
                let changed = facts.tree != previous.tree;
                self.remember_facts(facts.clone()).await;
                if let Err(err) = self.store.update_state(|s| s.facts = facts.clone()) {
                    tracing::debug!(error = %err.description(), "could not cache facts");
                }
                if changed || force_report {
                    self.sink.send(Report::Facts(facts)).await;
                }
                if changed && !previous.tree.is_null() {
                    self.top_up_secrets().await;
                }
            }
            Err(err) => tracing::warn!(error = %err.description(), "could not collect facts"),
        }
    }

    /// After facts change, more modules may select this host: ask the server
    /// for any declared secrets the host does not hold yet (it only returns
    /// those the newly selected modules reference).
    async fn top_up_secrets(&self) {
        if !self.connected.load(Ordering::SeqCst) {
            return;
        }
        let candidates: Vec<(RevisionId, Arc<Revision>)> = {
            let data = self.data.lock().await;
            [data.applied.clone(), data.assigned.clone()]
                .into_iter()
                .flatten()
                .filter_map(|id| data.revisions.get(&id).map(|r| (id, r.clone())))
                .collect()
        };
        for (id, rev) in candidates {
            let held = self.secrets_for(&id).await;
            let missing: Vec<String> = rev
                .fleet
                .secrets
                .keys()
                .filter(|name| !held.contains(name))
                .cloned()
                .collect();
            if missing.is_empty() {
                continue;
            }
            match self.transport.fetch_secrets(&id, &missing).await {
                Ok(wire) if !wire.is_empty() => {
                    tracing::info!(revision = %id.short(), count = wire.len(), "received additional secrets");
                    let mut data = self.data.lock().await;
                    self.cache_secrets(&mut data, &id, &wire);
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err.description(), "could not fetch additional secrets"),
            }
        }
    }

    async fn upgrade(&self, version: String) {
        tracing::info!(%version, "upgrading the agent");
        match self.updater.update(Some(&version)).await {
            Ok(UpdateOutcome::Updated { version }) => {
                tracing::info!(%version, "upgraded; exiting so the service manager restarts the agent");
                self.stop.cancel();
            }
            Ok(UpdateOutcome::UpToDate { version }) => {
                tracing::info!(%version, "already running the requested version")
            }
            Err(err) => tracing::error!(error = %err.description(), "the upgrade failed"),
        }
    }

    // ------------------------------------------------------------ local rollback watchdog

    async fn watchdog_ticker(self: Arc<Self>, queue: Queue, cancel: CancellationToken) {
        loop {
            let interval = self
                .watchdog
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(Watchdog::check_interval));
            match interval {
                None => tokio::select! {
                    _ = self.watchdog_wake.notified() => {}
                    _ = cancel.cancelled() => return,
                },
                Some(interval) => tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        if !self.watchdog_queued.swap(true, Ordering::SeqCst) {
                            queue.push(Op::WatchdogCheck, None);
                        }
                    }
                    _ = cancel.cancelled() => return,
                },
            }
        }
    }

    async fn disarm_watchdog(&self) {
        if let Ok(mut guard) = self.watchdog.lock() {
            *guard = None;
        }
        let mut data = self.data.lock().await;
        if data.agent.watchdog.take().is_some() {
            self.save_agent(&data);
        }
    }

    async fn watchdog_check(&self) {
        self.watchdog_queued.store(false, Ordering::SeqCst);
        let Some(mut watchdog) = self.watchdog.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        let connected = self.connected.load(Ordering::SeqCst);
        if !connected && watchdog.state.on_health {
            match self.ensure_revision(&watchdog.state.from, None).await {
                Ok(rev) => {
                    let ctx = self
                        .context(&watchdog.state.from, None, watchdog.state.waived_checks.clone(), false)
                        .await;
                    match self.runner.health(rev, &ctx, HealthScope::Bake, None).await {
                        Ok(report) => watchdog.observe(&report, Instant::now()),
                        Err(err) => tracing::warn!(error = %err.description(), "the watchdog could not run health checks"),
                    }
                }
                Err(err) => tracing::warn!(error = %err.description(), "the watchdog could not load the applied revision"),
            }
        }
        let verdict = watchdog.decide(self.connected.load(Ordering::SeqCst), Instant::now());
        // A Welcome may have disarmed the watchdog while the checks ran.
        let still_armed = match self.watchdog.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(current) if current.state == watchdog.state => {
                    *current = watchdog.clone();
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        if !still_armed {
            return;
        }
        match verdict {
            Verdict::Wait => {}
            Verdict::Disarm => {
                tracing::info!(revision = %watchdog.state.from.short(), "the local rollback window passed; the watchdog is disarmed");
                self.disarm_watchdog().await;
            }
            Verdict::RollBack { reason } => {
                self.disarm_watchdog().await;
                self.local_rollback(watchdog, reason).await;
            }
        }
    }

    async fn local_rollback(&self, watchdog: Watchdog, reason: String) {
        let from = watchdog.state.from.clone();
        let to = watchdog.state.to.clone();
        tracing::warn!(from = %from.short(), to = %to.short(), %reason, "rolling back to the cached stable revision");
        let result = async {
            let rev = self.ensure_revision(&to, None).await?;
            let ctx = self.context(&to, None, Vec::new(), true).await;
            let (plan, facts) = self.runner.plan(rev.clone(), &ctx).await?;
            self.remember_facts(facts).await;
            Ok::<_, buckle_core::Error>(self.run_apply(rev, &ctx, &plan, false).await)
        }
        .await;
        let (succeeded, reason) = match result {
            Ok(outcome) if outcome.succeeded => (true, reason),
            Ok(outcome) => (
                false,
                format!(
                    "{reason}; the rollback itself failed: {}",
                    outcome.error.unwrap_or_else(|| "a module failed".into())
                ),
            ),
            Err(err) => (
                false,
                format!("{reason}; the rollback could not run: {}", err.description()),
            ),
        };
        if !succeeded {
            tracing::error!(%reason, "the local rollback failed; the host needs attention");
        }
        let record = LocalRollbackRecord {
            from,
            to,
            reason,
            modules: Vec::new(),
            evidence: watchdog.evidence().to_vec(),
            at: chrono::Utc::now(),
            succeeded,
        };
        if self.connected.load(Ordering::SeqCst)
            && self.sink.send(Report::LocalRollback(record.clone())).await
        {
            return;
        }
        let mut data = self.data.lock().await;
        data.agent.pending_rollbacks.push(record);
        self.save_agent(&data);
    }
}

fn op_for(instruction: Instruction) -> Option<Op> {
    Some(match instruction {
        Instruction::Assign {
            revision,
            stable,
            secrets,
            tree_digest,
        } => Op::Assign {
            revision,
            stable,
            secrets,
            tree_digest,
            desired: true,
        },
        Instruction::RequestPlan { revision, select } => Op::Plan { revision, select },
        Instruction::Apply {
            revision,
            plan_digest,
            waived_checks,
            force,
            traceparent,
        } => Op::Apply {
            revision,
            plan_digest,
            waived_checks,
            force,
            traceparent,
        },
        Instruction::Rollback {
            revision,
            reason,
            traceparent,
        } => Op::Rollback {
            revision,
            reason,
            traceparent,
        },
        Instruction::RunHealth {
            request_id,
            scope,
            modules,
            until,
            traceparent,
        } => Op::Health {
            request_id,
            scope,
            modules,
            until,
            traceparent,
        },
        Instruction::Upgrade { version } => Op::Upgrade { version },
        Instruction::Welcome(_) | Instruction::Ping | Instruction::Rejected { .. } => return None,
    })
}
