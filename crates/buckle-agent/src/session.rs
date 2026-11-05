//! The session loop: connect, say Hello, follow instructions, report, and
//! reconnect with backoff; plus the drift pass, heartbeats, facts refresh and
//! the local rollback watchdog.
//!
//! Operations (assign, plan, apply, rollback, health, drift, facts, upgrade,
//! watchdog checks) run one at a time on a worker task, in the order they
//! were queued, so the session keeps heartbeating while an apply runs and
//! `busy` in the heartbeat reflects the worker. Plan, health, apply and
//! rollback requests queued by a session that has since ended are skipped
//! without a report: the server re-sends them after the reconnect (an apply
//! with the same plan digest and approval token, unless Hello shows the target
//! revision applied or the apply's result still unreported). An apply that
//! has started runs to completion and its result is durable. Assignments and
//! upgrades run regardless.

use crate::backoff::Backoff;
use crate::coalesce;
use crate::config::AgentConfig;
use crate::runner::{RunContext, Runner};
use crate::sink::ReportSink;
use crate::store::{AgentState, PendingReboot, Store};
use crate::transport::grpc::set_parent_from_traceparent;
use crate::transport::{
    DurableReport, Hello, Instruction, LocalRollbackRecord, Report, RevisionSource, Session,
    Transport, UpgradeReport, Welcome, WireSecret,
};
use crate::update::{UpdateOutcome, Updater};
use crate::watchdog::{Verdict, Watchdog};
use buckle_core::engine::{ApplyOutcome, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use buckle_core::plan::{Plan, RevisionId};
use buckle_core::secrets::{Fingerprint, InstallKey, Secret, SecretStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod targets;

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
    /// Cap of the backoff after the server refuses the host outright
    /// (`Rejected { retry: false }`): the agent keeps running and retries,
    /// from `backoff_min` doubling to this (1 h).
    pub rejected_backoff_max: Duration,
    /// Minimum spacing between ApplyEvent messages (100 ms, i.e. 10/s).
    pub event_spacing: Duration,
    /// How often shutdown logs that it is still waiting for the running
    /// operation (it waits for it to finish however long it takes; queued
    /// operations are dropped).
    pub shutdown_grace: Duration,
    /// How long a new session may go without a Welcome before the agent
    /// gives up on it and reconnects with backoff (30 s).
    pub welcome_timeout: Duration,
    /// How long the agent waits for the server to acknowledge the result
    /// that announced a reboot before rebooting anyway (60 s). The result is
    /// durable: it is re-sent after the reboot.
    pub reboot_ack_grace: Duration,
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
            rejected_backoff_max: Duration::from_secs(3600),
            event_spacing: coalesce::EVENT_SPACING,
            shutdown_grace: Duration::from_secs(60),
            welcome_timeout: Duration::from_secs(30),
            reboot_ack_grace: Duration::from_secs(60),
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
        approval_digest: Option<String>,
        waived_checks: Vec<String>,
        force: bool,
        traceparent: Option<String>,
        select: Option<String>,
        rollout_id: Option<String>,
        phase: Option<String>,
    },
    Rollback {
        revision: RevisionId,
        reason: String,
        rollout_id: Option<String>,
        traceparent: Option<String>,
    },
    Health {
        request_id: String,
        scope: HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        traceparent: Option<String>,
    },
    /// Runs an operator's action (RunAction).
    Action {
        action_id: String,
        module: String,
        action: String,
        revision: RevisionId,
        traceparent: Option<String>,
    },
    Drift,
    Facts {
        force_report: bool,
    },
    Upgrade {
        version: String,
        when_idle: bool,
    },
    WatchdogCheck,
    /// Carries out a pending reboot (one a restart interrupted).
    Reboot,
    /// Work for a cluster target this agent executes for.
    Target {
        target: String,
        instruction: Box<Instruction>,
    },
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
    /// Persisted in agent.json: applied and stable revision, facts, …
    agent: AgentState,
    assigned: Option<RevisionId>,
    /// The selector each recent plan digest was made with (only selected
    /// plans are remembered).
    selects: HashMap<String, String>,
    /// The facts tree last reported to a server.
    reported_facts: Option<serde_json::Value>,
    /// Tarball digests the server named for revisions (Assign, Welcome):
    /// every fetch of those revisions is verified against them.
    tree_digests: HashMap<RevisionId, String>,
    /// The latest targeted Assigns, per cluster target and revision.
    assignments: crate::cluster::Assignments,
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
    /// The revision an Apply is running for, for Hello.
    applying: Mutex<Option<RevisionId>>,
    /// The span of the current (or last) session, which agent-initiated
    /// passes follow from.
    session_span: Mutex<Option<tracing::Span>>,
    /// Shutting down (or restarting after an upgrade): queued operations are
    /// dropped; the running one finishes.
    stopping: AtomicBool,
    /// The server refused the stored per-host token: present the bootstrap
    /// token instead (a forgotten host registers afresh, design §08).
    use_bootstrap: AtomicBool,
    /// The identity of the current boot (read on start).
    boot_id: Mutex<Option<String>>,
    /// Signalled whenever the server acknowledges reports.
    acked: Notify,
    /// Runs the cluster targets this host executes for.
    executor: crate::cluster::Executor,
    /// Cluster credentials tasks ask for (`command.run` `identity`).
    credentials: Arc<crate::identity::Credentials>,
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

/// Labels typed the way the server types them (`"true"` is a boolean), so
/// module `when` selectors agree on both sides.
fn labels_json(labels: &indexmap::IndexMap<String, String>) -> buckle_core::selectors::Labels {
    buckle_core::selectors::typed_labels(labels)
}

/// `agent.yaml` vars as JSON values; a value JSON cannot represent is dropped with a warning.
fn vars_json(vars: &indexmap::IndexMap<String, serde_yaml::Value>) -> buckle_core::selectors::Vars {
    vars.iter()
        .filter_map(|(k, v)| match serde_json::to_value(v) {
            Ok(v) => Some((k.clone(), v)),
            Err(err) => {
                tracing::warn!(var = %k, error = %err, "ignoring an agent var that cannot be represented as JSON");
                None
            }
        })
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
        error_detail: Some(buckle_core::error::ErrorReport::of(error)),
        removals_run: Vec::new(),
        started_at: None,
        finished_at: None,
        reboot: None,
    }
}

/// The outcome of an action that failed before it ran.
pub(crate) fn action_failed(error: &buckle_core::Error, started: Instant) -> ApplyOutcome {
    ApplyOutcome {
        plan_digest: buckle_core::Digest::of(b""),
        not_started: Vec::new(),
        ..failed_outcome(
            &Plan::new(RevisionId::new(""), "", vec![], vec![], vec![]),
            error,
            started,
        )
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
            status: HealthStatus::unknown(format!(
                "the checks could not run: {}",
                error.description()
            )),
            at: chrono::Utc::now(),
            duration_ms: 0,
        }],
    }
}

/// When each health check is due while checks repeat (bake, or any request
/// with `until`): every check runs at its own declared interval (30 s when it
/// declares none). The engine runs a module's checks together, so a module
/// runs whenever one of its checks is due and only the due checks' results
/// count and are reported.
#[derive(Debug, Default)]
struct CheckSchedule {
    /// `module/check` → (interval, next due).
    checks: HashMap<String, (Duration, Instant)>,
}

impl CheckSchedule {
    const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

    fn interval_of(revision: &Revision, module: &str, check: &str) -> Duration {
        revision
            .modules
            .iter()
            .find(|m| m.config.name == module)
            .and_then(|m| m.config.health.iter().find(|h| h.display_name() == check))
            .map(|h| h.interval)
            .unwrap_or(Self::DEFAULT_INTERVAL)
            .max(Duration::from_secs(1))
    }

    /// Keeps the results that were due at `now` (and checks seen for the
    /// first time), and schedules their next run.
    fn take_due(
        &mut self,
        revision: &Revision,
        report: HealthReport,
        now: Instant,
    ) -> HealthReport {
        let results = report
            .results
            .into_iter()
            .filter(|r| {
                let key = format!("{}/{}", r.module, r.check);
                let interval = Self::interval_of(revision, &r.module, &r.check);
                match self.checks.get_mut(&key) {
                    Some((_, due)) if *due > now => false,
                    Some((_, due)) => {
                        *due = now + interval;
                        true
                    }
                    None => {
                        self.checks.insert(key, (interval, now + interval));
                        true
                    }
                }
            })
            .collect();
        HealthReport {
            scope: report.scope,
            results,
        }
    }

    /// The modules with a check due at `now`.
    fn due_modules(&self, now: Instant) -> Vec<String> {
        let mut modules: Vec<String> = self
            .checks
            .iter()
            .filter(|(_, (_, due))| *due <= now)
            .filter_map(|(key, _)| key.split_once('/').map(|(m, _)| m.to_owned()))
            .collect();
        modules.sort();
        modules.dedup();
        modules
    }

    /// When the next check is due, if any check is known.
    fn next_due(&self) -> Option<Instant> {
        self.checks.values().map(|(_, due)| *due).min()
    }
}

/// A filt-rs task selector matching every task of `modules`.
fn module_selector(modules: &[String]) -> String {
    modules
        .iter()
        .map(|m| format!("module.name == {}", serde_json::Value::String(m.clone())))
        .collect::<Vec<_>>()
        .join(" || ")
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
        let executor =
            crate::cluster::Executor::new(transport.clone(), Arc::new(crate::cluster::Network));
        Ok(Self {
            shared: Arc::new(Shared {
                executor,
                credentials: Arc::new(crate::identity::Credentials::new(
                    transport.clone(),
                    reqwest::Client::new(),
                )),
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
                applying: Mutex::new(None),
                use_bootstrap: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                session_span: Mutex::new(None),
                boot_id: Mutex::new(None),
                acked: Notify::new(),
            }),
        })
    }

    /// Reaches cluster targets through `connect` rather than over the
    /// network (tests use an in-memory Nomad).
    pub fn with_cluster_connect(mut self, connect: Arc<dyn crate::cluster::Connect>) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.executor = crate::cluster::Executor::new(shared.transport.clone(), connect);
        }
        self
    }

    /// Runs until `shutdown` trips or an upgrade asks for a restart (exit
    /// code 0). A server that refuses the host outright does not stop the
    /// agent (a service manager would only restart it every few seconds):
    /// it backs off up to an hour, falling back to the bootstrap token when
    /// the per-host token was refused.
    pub async fn run(self, shutdown: CancellationToken) -> buckle_core::Result<i32> {
        let shared = self.shared;
        shared.load().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let queue = Queue {
            tx,
            len: Arc::new(AtomicUsize::new(0)),
        };
        let worker = tokio::spawn(Shared::worker(
            shared.clone(),
            rx,
            queue.len.clone(),
            queue.tx.downgrade(),
        ));
        let ticker_cancel = CancellationToken::new();
        let ticker = tokio::spawn(Shared::watchdog_ticker(
            shared.clone(),
            queue.clone(),
            ticker_cancel.clone(),
        ));
        if shared.data.lock().await.agent.pending_reboot.is_some() {
            // A reboot a restart interrupted: carry it out before anything else.
            queue.push(Op::Reboot, None);
        }
        queue.push(
            Op::Facts {
                force_report: false,
            },
            None,
        );

        let mut backoff = Backoff::new(shared.options.backoff_min, shared.options.backoff_max);
        let mut rejected_backoff = Backoff::new(
            shared.options.backoff_min,
            shared.options.rejected_backoff_max,
        );
        if !shared.options.backoff_jitter {
            backoff = backoff.without_jitter();
            rejected_backoff = rejected_backoff.without_jitter();
        }
        let result = loop {
            if shutdown.is_cancelled() || shared.stop.is_cancelled() {
                break Ok(0);
            }
            let hello = shared.hello().await;
            tracing::debug!(?hello, "connecting");
            let used_bootstrap = hello.bootstrap;
            let connected = tokio::select! {
                result = shared.transport.connect(hello) => Some(result),
                _ = shutdown.cancelled() => None,
                _ = shared.stop.cancelled() => None,
            };
            let mut refused = false;
            match connected {
                None => break Ok(0),
                Some(Ok(session)) => {
                    let span = tracing::info_span!("agent.session", buckle.host = %shared.host_id);
                    if let Ok(mut guard) = shared.session_span.lock() {
                        *guard = Some(span.clone());
                    }
                    match shared
                        .clone()
                        .drive(session, &queue, &shutdown)
                        .instrument(span)
                        .await
                    {
                        SessionEnd::Shutdown => break Ok(0),
                        SessionEnd::Rejected {
                            reason,
                            retry: false,
                        } => {
                            refused = true;
                            shared.refused(&reason, used_bootstrap);
                        }
                        SessionEnd::Rejected {
                            reason,
                            retry: true,
                        } => {
                            tracing::warn!(%reason, "the server rejected the session; retrying");
                        }
                        SessionEnd::Closed { welcomed } => {
                            if welcomed {
                                backoff.reset();
                                rejected_backoff.reset();
                            }
                        }
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err.description(), "could not connect to the server");
                }
            }
            // A refusal that the bootstrap token may get past is retried on
            // the normal schedule; any other is retried slowly.
            let delay = if refused && !shared.use_bootstrap.load(Ordering::SeqCst) {
                rejected_backoff.next_delay()
            } else {
                backoff.next_delay()
            };
            tracing::info!(delay = ?delay, "reconnecting after a delay");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.cancelled() => break Ok(0),
                _ = shared.stop.cancelled() => break Ok(0),
            }
        };

        ticker_cancel.cancel();
        let _ = ticker.await;
        // Take no new operations, but let a running one (an apply above all)
        // finish: stopping mid-apply leaves the host half-changed.
        shared.stopping.store(true, Ordering::SeqCst);
        drop(queue);
        let mut worker = worker;
        let waiting_since = Instant::now();
        loop {
            match tokio::time::timeout(shared.options.shutdown_grace, &mut worker).await {
                Ok(_) => break,
                Err(_) => tracing::warn!(
                    waited = ?waiting_since.elapsed(),
                    busy = shared.busy.load(Ordering::SeqCst),
                    "waiting for the running operation to finish before exiting"
                ),
            }
        }
        result
    }
}

impl Shared {
    async fn load(&self) -> buckle_core::Result<()> {
        // state.cbor belongs to the engine; it is only read here, to adopt
        // the bookkeeping older agents kept in it.
        let state = self.store.load_state()?;
        let mut agent = self.store.load_agent_state();
        if agent.applied_revision.is_none() {
            agent.applied_revision = state.applied_revision;
        }
        if agent.stable_revision.is_none() {
            agent.stable_revision = state.stable_revision;
        }
        if agent.facts.tree.is_null() && !state.facts.tree.is_null() {
            agent.facts = state.facts;
        }
        agent.migrate_legacy();
        let boot_id = self.runner.boot_id().await;
        if let Ok(mut guard) = self.boot_id.lock() {
            *guard = boot_id.clone();
        }
        if let Some(pending) = agent.pending_reboot.clone() {
            let rebooted = match (&pending.request.boot_id, &boot_id) {
                (Some(then), Some(now)) => then != now,
                // No boot ids to compare: a reboot that was started is taken
                // as done.
                _ => pending.issued_at.is_some(),
            };
            if rebooted {
                tracing::info!(by = ?pending.request.requested_by, "back from the reboot the last run asked for");
                agent.pending_reboot = None;
                // The local rollback window counts from the host being back.
                if let Some(w) = agent.watchdog.as_mut() {
                    w.deadline_at = chrono::Utc::now()
                        + chrono::Duration::from_std(w.after).unwrap_or(chrono::Duration::MAX);
                }
                if let Err(err) = self.store.save_agent_state(&agent) {
                    tracing::error!(error = %err.description(), "could not persist the agent's session state");
                }
            }
        }
        if let Some(persisted) = agent.watchdog.clone() {
            tracing::info!(from = %persisted.from.short(), to = %persisted.to.short(), "resuming the local rollback watchdog");
            if let Ok(mut guard) = self.watchdog.lock() {
                *guard = Some(Watchdog::resume(persisted));
            }
        }
        let mut data = self.data.lock().await;
        data.agent = agent;
        Ok(())
    }

    /// The server refused the host outright. When it refused the stored
    /// per-host token (the host may have been forgotten) and a bootstrap
    /// token is configured, the next attempt presents the bootstrap token so
    /// the host registers afresh; when the bootstrap token was refused too,
    /// the next attempt goes back to the stored token.
    fn refused(&self, reason: &str, used_bootstrap: bool) {
        let can_fall_back =
            self.config.bootstrap_token.is_some() && matches!(self.store.read_token(), Ok(Some(_)));
        if !used_bootstrap && can_fall_back {
            tracing::warn!(%reason, "the server refused this host's token; registering again with the bootstrap token");
            self.use_bootstrap.store(true, Ordering::SeqCst);
        } else {
            self.use_bootstrap.store(false, Ordering::SeqCst);
            tracing::error!(%reason, "the server refused this host; retrying with backoff (ask a Buckle admin to approve or re-enrol it)");
        }
    }

    fn save_agent(&self, data: &Data) {
        if let Err(err) = self.store.save_agent_state(&data.agent) {
            tracing::error!(error = %err.description(), "could not persist the agent's session state");
        }
    }

    async fn hello(&self) -> Hello {
        let (workload_token, workload_jwks) = self.workload_identity().await;
        let data = self.data.lock().await;
        let issued = self.store.read_token().unwrap_or_else(|err| {
            tracing::warn!(error = %err.description(), "could not read the per-host token");
            None
        });
        let issued = issued.filter(|_| {
            !(self.use_bootstrap.load(Ordering::SeqCst) && self.config.bootstrap_token.is_some())
        });
        let (token, bootstrap) = match issued {
            Some(t) => (Some(t), false),
            None => (
                self.config.bootstrap_token.clone(),
                self.config.bootstrap_token.is_some(),
            ),
        };
        let mut unreported: Vec<RevisionId> = data
            .agent
            .unacked_reports
            .iter()
            .filter_map(DurableReport::apply_revision)
            .cloned()
            .collect();
        if let Some(running) = self.applying.lock().ok().and_then(|g| g.clone()) {
            unreported.push(running);
        }
        unreported.sort();
        unreported.dedup();
        Hello {
            unreported_apply_revisions: unreported,
            host_id: self.host_id.clone(),
            agent_version: self.options.agent_version.clone(),
            labels: self.config.labels.clone(),
            applied_revision: data.agent.applied_revision.clone(),
            last_plan_digest: data.agent.last_plan_digest.clone(),
            token,
            platform: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            bootstrap,
            boot_id: self.current_boot(),
            workload_token,
            workload_jwks,
        }
    }

    /// The workload identity token (read afresh: the scheduler rotates it)
    /// and the cluster's key set, for an in-cluster executor.
    async fn workload_identity(&self) -> (Option<String>, Option<String>) {
        let Some(wi) = &self.config.workload_identity else {
            return (None, None);
        };
        let token = match tokio::fs::read_to_string(&wi.token_file).await {
            Ok(t) if !t.trim().is_empty() => Some(t.trim().to_owned()),
            Ok(_) => {
                tracing::error!(file = %wi.token_file.display(), "the workload identity token file is empty");
                None
            }
            Err(err) => {
                tracing::error!(file = %wi.token_file.display(), error = %err, "could not read the workload identity token");
                None
            }
        };
        let jwks = match &wi.jwks_url {
            None => None,
            Some(url) => {
                let got = async {
                    reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()?
                        .get(url)
                        .send()
                        .await?
                        .error_for_status()?
                        .text()
                        .await
                }
                .await;
                got.map_err(|err| tracing::warn!(%url, error = %err, "could not read the cluster's signing keys to relay"))
                    .ok()
            }
        };
        (token, jwks)
    }

    fn current_boot(&self) -> Option<String> {
        self.boot_id.lock().ok().and_then(|g| g.clone())
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
        let mut heartbeat =
            tokio::time::interval_at(start + self.options.heartbeat, self.options.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let drift_every = self.config.drift.interval.max(Duration::from_secs(1));
        let mut drift = tokio::time::interval_at(start + drift_every, drift_every);
        drift.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut facts = tokio::time::interval_at(
            start + self.options.facts_interval,
            self.options.facts_interval,
        );
        facts.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let welcome_deadline = tokio::time::sleep(self.options.welcome_timeout);
        tokio::pin!(welcome_deadline);

        let end = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break SessionEnd::Shutdown,
                _ = &mut welcome_deadline, if !welcomed => {
                    tracing::warn!(timeout = ?self.options.welcome_timeout, "the server sent no Welcome; reconnecting");
                    break SessionEnd::Closed { welcomed: false };
                }
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
                    Some(Instruction::Ack { report_ids }) => self.on_ack(&report_ids).await,
                    Some(Instruction::HostUpdate(update)) => self.on_host_update(update, queue).await,
                    // Work for a cluster target this host executes for. As
                    // for the host's own work, all but the Assign belong to
                    // the session that asked.
                    Some(other) if other.target().is_some() => {
                        let target = other.target().unwrap_or_default().to_owned();
                        // An action runs even after a reconnect: its run is
                        // open on the server until its (durable) result.
                        let bound = !matches!(
                            other,
                            Instruction::Assign { .. } | Instruction::RunAction { .. }
                        );
                        queue.push(
                            Op::Target { target, instruction: Box::new(other) },
                            bound.then_some(generation),
                        );
                    }
                    Some(other) => {
                        if let Some(op) = op_for(other) {
                            // Plans, health answers, applies and rollbacks
                            // belong to the session that asked: one that has
                            // not started when its session ends is skipped,
                            // and the server re-sends it after the reconnect
                            // (an apply that never ran has no result and no
                            // watchdog to arm). Assignments and upgrades
                            // still run.
                            let bound = matches!(
                                op,
                                Op::Plan { .. }
                                    | Op::Health { .. }
                                    | Op::Apply { .. }
                                    | Op::Rollback { .. }
                            );
                            queue.push(op, bound.then_some(generation));
                        }
                    }
                },
                _ = heartbeat.tick() => if welcomed {
                    let applied = self.data.lock().await.agent.applied_revision.clone();
                    self.sink.send(Report::Heartbeat {
                        applied_revision: applied,
                        busy: self.busy.load(Ordering::SeqCst),
                    }).await;
                },
                _ = drift.tick() => if welcomed
                    && !self.busy.load(Ordering::SeqCst)
                    && queue.is_empty()
                    && self.data.lock().await.agent.applied_revision.is_some()
                {
                    queue.push(Op::Drift, None);
                },
                _ = facts.tick() => queue.push(Op::Facts { force_report: false }, None),
            }
        };
        self.connected.store(false, Ordering::SeqCst);
        // Operations this session queued are now orphaned, even while no
        // new session can be opened.
        self.generation.fetch_add(1, Ordering::SeqCst);
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
                Ok(()) => {
                    self.use_bootstrap.store(false, Ordering::SeqCst);
                    tracing::info!("stored the per-host token issued by the server")
                }
                Err(err) => {
                    tracing::error!(error = %err.description(), "could not store the per-host token")
                }
            }
        }
        let pending = {
            let mut data = self.data.lock().await;
            self.adopt_host(
                &mut data,
                &welcome.status,
                &welcome.labels,
                welcome.stable_revision.as_ref(),
                welcome.stable_tree_digest.as_deref(),
                &welcome.secrets,
                queue,
            );
            if let Ok(mut guard) = self.watchdog.lock()
                && let Some(w) = guard.take()
            {
                tracing::info!(revision = %w.state.from.short(), "the session came back; the local rollback watchdog is disarmed");
                data.agent.watchdog = None;
            }
            self.save_agent(&data);
            data.agent
                .unacked_reports
                .iter()
                .filter_map(DurableReport::to_report)
                .collect::<Vec<_>>()
        };
        self.connected.store(true, Ordering::SeqCst);

        // Durable reports stay in agent.json until the server acknowledges
        // them; the server stores each report id once, so re-sending after
        // every Welcome is safe.
        if !pending.is_empty() {
            tracing::info!(count = pending.len(), "re-sending unacknowledged reports");
        }
        for report in pending {
            self.sink.send(report).await;
        }
        queue.push(Op::Facts { force_report: true }, None);
    }

    /// Takes on what the server says about this host (from Welcome or a
    /// HostUpdate): status, effective labels, the stable revision and its
    /// secrets (fetching the stable tree if it is not cached).
    #[allow(clippy::too_many_arguments)]
    fn adopt_host(
        &self,
        data: &mut Data,
        status: &str,
        labels: &indexmap::IndexMap<String, String>,
        stable: Option<&RevisionId>,
        stable_tree_digest: Option<&str>,
        secrets: &[WireSecret],
        queue: &Queue,
    ) {
        data.agent.labels = labels.clone();
        data.agent.status = Some(status.to_owned()).filter(|s| !s.is_empty());
        if let Some(stable) = stable {
            if data.agent.stable_revision.as_ref() != Some(stable) {
                data.agent.stable_tree_digest = None;
            }
            data.agent.stable_revision = Some(stable.clone());
            if let Some(digest) = stable_tree_digest {
                data.agent.stable_tree_digest = Some(digest.to_owned());
                data.tree_digests.insert(stable.clone(), digest.to_owned());
            }
            if !secrets.is_empty() {
                self.cache_secrets(data, stable, secrets);
            }
            // Only an active host may fetch revisions; a pending or paused
            // one caches the stable tree once it is active (HostUpdate).
            if status == "active" && !self.store.has_revision(stable) {
                queue.push(
                    Op::Assign {
                        revision: stable.clone(),
                        stable: true,
                        secrets: Vec::new(),
                        tree_digest: stable_tree_digest.map(str::to_owned),
                        desired: false,
                    },
                    None,
                );
            }
        }
    }

    async fn on_host_update(&self, update: crate::transport::HostUpdate, queue: &Queue) {
        tracing::info!(status = %update.status, "the server updated this host's record");
        let mut data = self.data.lock().await;
        self.adopt_host(
            &mut data,
            &update.status,
            &update.labels,
            update.stable_revision.as_ref(),
            update.stable_tree_digest.as_deref(),
            &update.secrets,
            queue,
        );
        self.save_agent(&data);
    }

    /// Links an agent-initiated root span to the current (or last) session.
    fn follow_session(&self, span: &tracing::Span) {
        if let Some(session) = self.session_span.lock().ok().and_then(|g| g.clone()) {
            span.follows_from(&session);
        }
    }

    fn set_applying(&self, revision: Option<RevisionId>) {
        if let Ok(mut guard) = self.applying.lock() {
            *guard = revision;
        }
    }

    async fn on_ack(&self, ids: &[u64]) {
        let mut data = self.data.lock().await;
        if data.agent.acknowledge(ids) {
            tracing::debug!(?ids, "the server acknowledged reports");
            self.save_agent(&data);
        }
        drop(data);
        self.acked.notify_waiters();
    }

    /// Records `report` as durable (kept until acknowledged) and sends it.
    async fn send_durable(&self, report: Report) {
        if let Some(durable) = DurableReport::from_report(&report) {
            let mut data = self.data.lock().await;
            data.agent.keep_report(durable);
            self.save_agent(&data);
        }
        self.sink.send(report).await;
    }

    async fn next_report_id(&self) -> u64 {
        let mut data = self.data.lock().await;
        let id = data.agent.next_report_id();
        self.save_agent(&data);
        id
    }

    async fn worker(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<Queued>,
        len: Arc<AtomicUsize>,
        requeue: mpsc::WeakUnboundedSender<Queued>,
    ) {
        while let Some(queued) = rx.recv().await {
            len.fetch_sub(1, Ordering::SeqCst);
            // An upgrade sent `when_idle` waits until nothing else is queued
            // (design §08: "install … when idle").
            if let Op::Upgrade {
                when_idle: true, ..
            } = &queued.op
                && len.load(Ordering::SeqCst) > 0
                && !self.stopping.load(Ordering::SeqCst)
                && let Some(tx) = requeue.upgrade()
            {
                tracing::debug!("deferring the upgrade until the agent is idle");
                len.fetch_add(1, Ordering::SeqCst);
                if tx.send(queued).is_err() {
                    len.fetch_sub(1, Ordering::SeqCst);
                }
                continue;
            }
            if self.stopping.load(Ordering::SeqCst) || self.stop.is_cancelled() {
                tracing::info!(op = ?queued.op, "not starting an operation: the agent is stopping");
                continue;
            }
            if let Some(generation) = queued.generation
                && generation != self.generation.load(Ordering::SeqCst)
            {
                match &queued.op {
                    Op::Apply { revision, .. } | Op::Rollback { revision, .. } => {
                        tracing::info!(revision = %revision.short(), "skipping an apply whose session ended before it started; the server re-sends it");
                    }
                    op => {
                        tracing::debug!(
                            ?op,
                            "skipping an operation queued by a session that has ended"
                        )
                    }
                }
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
                let span = tracing::info_span!(
                    "agent.plan",
                    buckle.host = %self.host_id,
                    buckle.revision = %revision.0,
                    buckle.plan.digest = tracing::field::Empty
                );
                self.plan(revision, select).instrument(span).await
            }
            Op::Apply {
                revision,
                plan_digest,
                approval_digest,
                waived_checks,
                force,
                traceparent,
                select,
                rollout_id,
                phase,
            } => {
                let span = tracing::info_span!(
                    "agent.apply",
                    buckle.host = %self.host_id,
                    buckle.revision = %revision.0,
                    buckle.plan.digest = plan_digest.as_deref().unwrap_or(""),
                    buckle.rollout = rollout_id.as_deref().unwrap_or(""),
                    buckle.phase = phase.as_deref().unwrap_or(""),
                    force
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.set_applying(Some(revision.clone()));
                self.apply(
                    revision,
                    plan_digest,
                    approval_digest,
                    select,
                    waived_checks,
                    force,
                )
                .instrument(span)
                .await;
                self.set_applying(None);
                self.reboot_if_pending().await;
            }
            Op::Rollback {
                revision,
                reason,
                rollout_id,
                traceparent,
            } => {
                let span = tracing::info_span!(
                    "agent.apply",
                    buckle.host = %self.host_id,
                    buckle.revision = %revision.0,
                    buckle.rollout = rollout_id.as_deref().unwrap_or(""),
                    rollback = true
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.rollback(revision, reason).instrument(span).await;
                self.reboot_if_pending().await;
            }
            Op::Health {
                request_id,
                scope,
                modules,
                until,
                traceparent,
            } => {
                let span = tracing::info_span!(
                    "agent.health",
                    buckle.host = %self.host_id,
                    buckle.revision = tracing::field::Empty,
                    scope = ?scope,
                    request = %request_id
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.health(request_id, scope, modules, until)
                    .instrument(span)
                    .await
            }
            Op::Drift => {
                // Agent-initiated work starts a fresh trace per pass that
                // follows from the session (design §13), so it exports
                // promptly rather than with a session that lives for days.
                let span = tracing::info_span!(
                    parent: None,
                    "agent.drift",
                    buckle.host = %self.host_id,
                    buckle.revision = tracing::field::Empty,
                    buckle.plan.digest = tracing::field::Empty
                );
                self.follow_session(&span);
                self.drift().instrument(span).await
            }
            Op::Action {
                action_id,
                module,
                action,
                revision,
                traceparent,
            } => {
                let span = tracing::info_span!(
                    "agent.action",
                    buckle.host = %self.host_id,
                    buckle.revision = %revision.0,
                    buckle.module = %module,
                    buckle.action = %action,
                    buckle.action_id = %action_id,
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.run_action(action_id, module, action, revision)
                    .instrument(span)
                    .await;
                self.reboot_if_pending().await;
            }
            Op::Facts { force_report } => self.facts(force_report).await,
            Op::Upgrade { version, .. } => self.upgrade(version).await,
            Op::WatchdogCheck => {
                let span = tracing::info_span!(
                    parent: None,
                    "agent.watchdog",
                    buckle.host = %self.host_id
                );
                self.follow_session(&span);
                self.watchdog_check().instrument(span).await;
                self.reboot_if_pending().await;
            }
            Op::Reboot => self.reboot_if_pending().await,
            Op::Target {
                target,
                instruction,
            } => self.target_op(target, *instruction).await,
        }
    }

    // ------------------------------------------------------------ revisions and secrets

    /// Seals secrets from the wire, keeping the server's fingerprints so plan
    /// digests agree across the fleet.
    fn import_secrets(&self, wire: &[WireSecret]) -> SecretStore {
        let mut store = SecretStore::new();
        for secret in wire {
            let imported = match Fingerprint::parse(&secret.fingerprint) {
                Ok(fingerprint) => SecretStore::import_fingerprinted([(
                    secret.name.clone(),
                    fingerprint,
                    secret.value.clone(),
                )]),
                Err(_) => {
                    tracing::warn!(secret = %secret.name, "the server sent a secret without a valid fingerprint; fingerprinting it locally");
                    String::from_utf8(secret.value.clone())
                        .map(|value| {
                            let mut one = SecretStore::new();
                            one.insert(Secret::new(secret.name.clone(), value, &self.install_key));
                            one
                        })
                        .map_err(|_| {
                            buckle_core::error::user(
                                format!("The secret '{}' is not UTF-8.", secret.name),
                                &["Store secrets as text."],
                            )
                        })
                }
            };
            match imported {
                Ok(one) => {
                    for s in one.iter() {
                        store.insert(s.clone());
                    }
                }
                Err(err) => {
                    tracing::warn!(secret = %secret.name, error = %err.description(), "ignoring a secret that could not be imported")
                }
            }
        }
        store
    }

    fn cache_secrets(&self, data: &mut Data, revision: &RevisionId, wire: &[WireSecret]) {
        let mut merged = match data.secrets.get(revision) {
            Some(existing) => (**existing).clone(),
            None => self
                .store
                .load_secrets(revision, &self.install_key)
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err.description(), "discarding an unreadable secret cache");
                    None
                })
                .unwrap_or_default(),
        };
        for secret in self.import_secrets(wire).iter() {
            merged.insert(secret.clone());
        }
        if let Err(err) = self
            .store
            .save_secrets(revision, &merged, &self.install_key)
        {
            tracing::error!(error = %err.description(), "could not cache secrets");
        }
        data.secrets.insert(revision.clone(), Arc::new(merged));
    }

    async fn secrets_for(&self, revision: &RevisionId) -> Arc<SecretStore> {
        let mut data = self.data.lock().await;
        if let Some(s) = data.secrets.get(revision) {
            return s.clone();
        }
        let store = match self.store.load_secrets(revision, &self.install_key) {
            Ok(Some(store)) => Arc::new(store),
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
        let tree_digest: Option<String> = {
            let mut data = self.data.lock().await;
            if let Some(r) = data.revisions.get(revision) {
                return Ok(r.clone());
            }
            match tree_digest {
                Some(digest) => {
                    data.tree_digests
                        .insert(revision.clone(), digest.to_owned());
                    Some(digest.to_owned())
                }
                None => data.tree_digests.get(revision).cloned().or_else(|| {
                    (data.agent.stable_revision.as_ref() == Some(revision))
                        .then(|| data.agent.stable_tree_digest.clone())
                        .flatten()
                }),
            }
        };
        let tree_digest = tree_digest.as_deref();
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
        let loaded = Arc::new(buckle_core::engine::loader::load_dir(
            &dir,
            revision.clone(),
        )?);
        let mut data = self.data.lock().await;
        let keep = self.keep_list(&data);
        data.revisions.retain(|id, _| keep.contains(id));
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
        let stable_revision = self.stable_revision_for(revision).await;
        // The engine renders a failing module's stable version with these
        // secrets too, so the stable revision's are merged in.
        let secrets = match &stable_revision {
            Some(stable) => self.rollback_secrets(&stable.id, revision).await,
            None => self.secrets_for(revision).await,
        };
        let data = self.data.lock().await;
        RunContext {
            host: buckle_core::selectors::HostIdentity {
                id: self.host_id.clone(),
                labels: labels_json(&self.config.labels),
            },
            server_labels: labels_json(&data.agent.labels),
            secrets,
            facts: data.agent.facts.clone(),
            select,
            waived_checks,
            force,
            stable_revision,
            vars: vars_json(&self.config.vars),
            drift: false,
            clusters: Some(Arc::new(crate::identity::TaskCredentials {
                credentials: self.credentials.clone(),
                revision: Some(revision.clone()),
            })),
        }
    }

    /// The cached stable revision to roll a failing module back to, when one is
    /// known, cached and not the revision being applied. Loads it on first use,
    /// outside the data lock.
    async fn stable_revision_for(
        &self,
        revision: &RevisionId,
    ) -> Option<Arc<buckle_core::engine::Revision>> {
        let stable = {
            let data = self.data.lock().await;
            let stable = data.agent.stable_revision.clone()?;
            if &stable == revision {
                return None;
            }
            if let Some(loaded) = data.revisions.get(&stable) {
                return Some(loaded.clone());
            }
            stable
        };
        if !self.store.has_revision(&stable) {
            tracing::debug!(stable = %stable.short(), "stable revision not cached; module rollback disabled for this run");
            return None;
        }
        let dir = match self.store.revision_dir(&stable) {
            Ok(dir) => dir,
            Err(err) => {
                tracing::warn!(error = %err.description(), "could not locate the cached stable revision");
                return None;
            }
        };
        let loaded = {
            let (dir, id) = (dir, stable.clone());
            tokio::task::spawn_blocking(move || buckle_core::engine::loader::load_dir(&dir, id))
                .await
                .map_err(|e| {
                    buckle_core::error::wrap_system(e, "Loading the stable revision panicked.", &[])
                })
                .and_then(|r| r)
        };
        match loaded {
            Ok(loaded) => {
                let loaded = Arc::new(loaded);
                self.data
                    .lock()
                    .await
                    .revisions
                    .entry(stable)
                    .or_insert_with(|| loaded.clone());
                Some(loaded)
            }
            Err(err) => {
                tracing::warn!(stable = %stable.short(), error = %err.description(), "the cached stable revision does not load; module rollback disabled for this run");
                None
            }
        }
    }

    async fn remember_facts(&self, facts: Facts) {
        self.remember_facts_and_report(facts, false).await
    }

    async fn remember_facts_and_report(&self, facts: Facts, force: bool) {
        let connected = self.connected.load(Ordering::SeqCst);
        let (report, top_up) = {
            let mut data = self.data.lock().await;
            if data.agent.facts != facts {
                data.agent.facts = facts;
                self.save_agent(&data);
            }
            let changed = data.reported_facts.as_ref() != Some(&data.agent.facts.tree);
            if !connected || !(changed || force) {
                (None, false)
            } else {
                let seen_before = data.reported_facts.is_some();
                data.reported_facts = Some(data.agent.facts.tree.clone());
                (Some(data.agent.facts.clone()), changed && seen_before)
            }
        };
        if let Some(facts) = report {
            self.sink.send(Report::Facts(facts)).await;
        }
        if top_up {
            self.top_up_secrets().await;
        }
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
                data.agent.stable_revision = Some(revision.clone());
                self.save_agent(&data);
            }
            if desired {
                data.assigned = Some(revision.clone());
            }
        }
        let fetched = self
            .ensure_revision(&revision, tree_digest.as_deref())
            .await;
        {
            // Prune whether or not the fetch worked: secrets cached for a
            // revision whose tree never arrived go too once it is no longer
            // wanted.
            let mut data = self.data.lock().await;
            let keep = self.keep_list(&data);
            let refs: Vec<&RevisionId> = keep.iter().collect();
            self.store.prune_revisions(&refs);
            data.secrets.retain(|id, _| keep.contains(id));
        }
        match fetched {
            Ok(_) => {
                tracing::info!(revision = %revision.short(), stable, "revision assigned");
            }
            Err(err) if !desired => {
                // Caching the stable revision for local rollback is the
                // agent's own business: a failure is retried with the next
                // Welcome or HostUpdate, and is not a plan failure of a
                // revision the server assigned.
                tracing::warn!(revision = %revision.short(), error = %err.description(), "could not cache the stable revision");
            }
            Err(err) => {
                tracing::error!(revision = %revision.short(), error = %err.description(), "could not fetch the assigned revision");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: None,
                    })
                    .await;
            }
        }
    }

    async fn plan(&self, revision: RevisionId, select: Option<String>) {
        let result = async {
            let rev = self.ensure_revision(&revision, None).await?;
            self.ensure_secrets(&revision, &rev).await;
            let ctx = self
                .context(&revision, select.clone(), Vec::new(), false)
                .await;
            self.runner.plan(rev, &ctx).await
        }
        .await;
        match result {
            Ok((plan, facts)) => {
                if let Some(select) = &select {
                    let mut data = self.data.lock().await;
                    if data.selects.len() >= 64 {
                        data.selects.clear();
                    }
                    data.selects.insert(plan.digest.to_string(), select.clone());
                }
                self.remember_facts(facts).await;
                self.remember_plan(&plan).await;
                tracing::Span::current().record("buckle.plan.digest", plan.digest.as_str());
                tracing::info!(digest = %plan.digest.short(), changes = plan.changes(), "planned");
                self.sink.send(Report::Plan(plan)).await;
            }
            Err(err) => {
                tracing::error!(error = %err.description(), "planning failed");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: None,
                    })
                    .await;
            }
        }
    }

    /// Applies `plan`. `report` sends ApplyEvents and the (durable)
    /// ApplyResult to the current session; `record` makes a success the
    /// applied revision (a module-scoped rollback applies part of the stable
    /// revision and records nothing).
    async fn run_apply(
        &self,
        rev: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        report: bool,
        record: bool,
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
        let mut outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                if let Some(stale) = buckle_core::engine::StalePlan::find(&err) {
                    // The host changed between the agent's plan and the
                    // engine's own check: nothing was applied. Report the
                    // fresh plan for re-approval, as for a digest mismatch.
                    tracing::warn!(approved = %stale.planned, fresh = %stale.fresh, "the engine refused a stale plan");
                    if report {
                        self.report_stale(rev.clone(), ctx, plan).await;
                    }
                    return failed_outcome(plan, &err, started);
                }
                tracing::error!(error = %err.description(), "the apply failed");
                failed_outcome(plan, &err, started)
            }
        };
        let applied = {
            let mut data = self.data.lock().await;
            if record {
                if outcome.succeeded {
                    data.agent.applied_revision = Some(rev.id.clone());
                    data.agent.waived_checks = ctx.waived_checks.clone();
                }
                data.agent.rolled_back_modules = outcome.rolled_back_modules.clone();
                data.agent.last_plan_digest = Some(plan.digest.to_string());
                self.save_agent(&data);
            }
            data.agent.applied_revision.clone()
        };
        if let Some(reboot) = outcome.reboot.as_mut() {
            reboot.boot_id = self.current_boot();
        }
        if report {
            let report_id = self.next_report_id().await;
            if let Some(reboot) = &outcome.reboot {
                self.schedule_reboot(report_id, reboot.clone()).await;
            }
            self.send_durable(Report::ApplyResult {
                report_id,
                revision: rev.id.clone(),
                outcome: outcome.clone(),
                applied_revision: applied,
            })
            .await;
        }
        outcome
    }

    /// Plans and runs an operator's action on the host and reports it
    /// (durably). The applied revision, state and watchdog are untouched.
    async fn run_action(
        &self,
        action_id: String,
        module: String,
        action: String,
        revision: RevisionId,
    ) {
        let started = Instant::now();
        let prepared = async {
            let rev = self.ensure_revision(&revision, None).await?;
            self.ensure_secrets(&revision, &rev).await;
            let mut ctx = self.context(&revision, None, Vec::new(), false).await;
            // An action never rolls a module back.
            ctx.stable_revision = None;
            let (plan, facts) = self
                .runner
                .plan_action(rev.clone(), &ctx, &module, &action)
                .await?;
            Ok::<_, buckle_core::Error>((rev, ctx, plan, facts))
        }
        .await;
        let mut outcome = match prepared {
            Err(err) => {
                tracing::warn!(error = %err.description(), "the action could not be planned");
                action_failed(&err, started)
            }
            Ok((rev, ctx, plan, facts)) => {
                self.remember_facts(facts).await;
                let (tx, rx) = mpsc::channel(256);
                let forwarder = tokio::spawn(coalesce::forward_for(
                    rx,
                    self.sink.clone(),
                    rev.id.clone(),
                    plan.digest.to_string(),
                    self.options.event_spacing,
                    None,
                    Some(action_id.clone()),
                ));
                let result = self
                    .runner
                    .run_action(rev, &ctx, &module, &action, &plan, tx)
                    .await;
                let _ = forwarder.await;
                result.unwrap_or_else(|err| {
                    tracing::error!(error = %err.description(), "the action failed");
                    failed_outcome(&plan, &err, started)
                })
            }
        };
        tracing::info!(
            succeeded = outcome.succeeded,
            changes = outcome.changes,
            "action finished"
        );
        if let Some(reboot) = outcome.reboot.as_mut() {
            reboot.boot_id = self.current_boot();
        }
        let report_id = self.next_report_id().await;
        if let Some(reboot) = &outcome.reboot {
            self.schedule_reboot(report_id, reboot.clone()).await;
        }
        self.send_durable(Report::ActionResult(crate::transport::ActionRun {
            report_id,
            action_id,
            module,
            action,
            revision,
            outcome,
        }))
        .await;
    }

    async fn apply(
        &self,
        revision: RevisionId,
        plan_digest: Option<String>,
        approval_digest: Option<String>,
        select: Option<String>,
        waived_checks: Vec<String>,
        force: bool,
    ) {
        let connected_before = self.connected.load(Ordering::SeqCst);
        // The approved plan was made with the RequestPlan's selector: the
        // server echoes it; older servers do not, so fall back to the one
        // this agent remembers planning that digest with.
        let select = match select {
            Some(select) => Some(select),
            None => match &plan_digest {
                Some(digest) => self.data.lock().await.selects.get(digest).cloned(),
                None => None,
            },
        };
        let prepared = async {
            let rev = self.ensure_revision(&revision, None).await?;
            self.ensure_secrets(&revision, &rev).await;
            let ctx = self
                .context(&revision, select.clone(), waived_checks.clone(), force)
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
                        target: None,
                    })
                    .await;
                return;
            }
        };
        self.remember_facts(facts).await;
        // The approval binds the apply: its intent digest when the server
        // sent one, else the full plan digest. `force` only overrides the
        // server's baseline hold, never this check; only a force apply (no
        // digest at all) skips it.
        let binding = match (&approval_digest, &plan_digest) {
            (Some(intent), _) => Some((intent, plan.intent_digest())),
            (None, Some(full)) => Some((full, plan.digest.clone())),
            (None, None) => None,
        };
        if let Some((approved, fresh)) = &binding
            && !crate::bundle::digests_match(approved, fresh.as_str())
        {
            tracing::warn!(approved = %approved, fresh = %fresh, "refusing a stale plan");
            self.remember_plan(&plan).await;
            // Named by the plan digest the operator saw, when there is one.
            let approved_digest = plan_digest.clone().unwrap_or_else(|| (*approved).clone());
            self.sink
                .send(Report::StalePlan {
                    plan,
                    approved_digest,
                })
                .await;
            return;
        }
        // The plan about to run differs from the one the server holds (a
        // force apply, or content that changed under an approved intent):
        // report it first so the server records what was really applied.
        if plan_digest
            .as_deref()
            .is_none_or(|d| !crate::bundle::digests_match(d, plan.digest.as_str()))
        {
            self.remember_plan(&plan).await;
            self.sink.send(Report::Plan(plan.clone())).await;
        }
        let outcome = self.run_apply(rev.clone(), &ctx, &plan, true, true).await;
        tracing::info!(
            succeeded = outcome.succeeded,
            changes = outcome.changes,
            "apply finished"
        );
        // An apply the engine refused outright (a stale plan) changed nothing.
        if outcome.succeeded || !outcome.module_results.is_empty() {
            self.maybe_arm_watchdog(&rev, connected_before, waived_checks)
                .await;
        }
    }

    /// Re-plans and reports the fresh plan as stale against `approved`.
    async fn report_stale(&self, rev: Arc<Revision>, ctx: &RunContext, approved: &Plan) {
        match self.runner.plan(rev.clone(), ctx).await {
            Ok((fresh, facts)) => {
                self.remember_facts(facts).await;
                self.remember_plan(&fresh).await;
                self.sink
                    .send(Report::StalePlan {
                        plan: fresh,
                        approved_digest: approved.digest.to_string(),
                    })
                    .await;
            }
            Err(err) => {
                self.sink
                    .send(Report::PlanFailed {
                        revision: rev.id.clone(),
                        error: err.description(),
                        target: None,
                    })
                    .await;
            }
        }
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
            tracing::debug!(
                "not arming the local rollback watchdog: there was no session before the apply"
            );
            return;
        }
        let mut data = self.data.lock().await;
        let Some(stable) = data.agent.stable_revision.clone() else {
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
                self.run_apply(rev, &ctx, &plan, true, true).await;
                // A server-driven rollback supersedes any local watchdog.
                self.disarm_watchdog().await;
            }
            Err(err) => {
                tracing::error!(error = %err.description(), "could not plan the rollback");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: None,
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
        let applied = self.data.lock().await.agent.applied_revision.clone();
        if let Some(r) = &applied {
            tracing::Span::current().record("buckle.revision", r.0.as_str());
        }
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
            Instant::now()
                + (until - chrono::Utc::now())
                    .to_std()
                    .unwrap_or(Duration::ZERO)
        });
        let cancel = self
            .session_cancel
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        let settle = rev.fleet.rollout.settle;
        let waived = self.data.lock().await.agent.waived_checks.clone();
        let mut schedule = CheckSchedule::default();
        // Required checks failing continuously during bake, since when.
        let mut failing_since: HashMap<String, Instant> = HashMap::new();
        let mut first = true;
        loop {
            let now = Instant::now();
            let targets = if first {
                modules.clone()
            } else {
                Some(schedule.due_modules(now))
            };
            first = false;
            let report = match self
                .runner
                .health(rev.clone(), &ctx, scope, targets.as_deref())
                .await
            {
                Ok(report) => schedule.take_due(&rev, report, now),
                Err(err) => health_error_report(scope, &err),
            };
            let report = self.scrub_report(&revision, report).await;
            let mut rollback: Option<(Vec<String>, Vec<HealthResult>)> = None;
            if scope == HealthScope::Bake {
                for r in &report.results {
                    let key = format!("{}/{}", r.module, r.check);
                    if r.counts_as_failure(&waived) {
                        failing_since.entry(key).or_insert(now);
                    } else {
                        failing_since.remove(&key);
                    }
                }
                let settled: Vec<HealthResult> = report
                    .failures(&waived)
                    .filter(|r| {
                        failing_since
                            .get(&format!("{}/{}", r.module, r.check))
                            .is_some_and(|since| now.duration_since(*since) >= settle)
                    })
                    .cloned()
                    .collect();
                if !settled.is_empty() {
                    let mut failed: Vec<String> =
                        settled.iter().map(|r| r.module.clone()).collect();
                    failed.sort();
                    failed.dedup();
                    rollback = Some((failed, settled));
                }
            }
            self.sink
                .send(Report::Health {
                    request_id: Some(request_id.clone()),
                    revision: Some(revision.clone()),
                    report,
                })
                .await;
            if let Some((failed, evidence)) = rollback {
                // Design §04/§07: a required check failing past settle after
                // its module applied rolls that module back to the stable
                // revision; the agent then makes no further changes and the
                // server decides (it pauses).
                self.module_rollback(&revision, failed, evidence, settle)
                    .await;
                break;
            }
            let Some(deadline) = deadline else { break };
            let Some(next) = schedule.next_due() else {
                break;
            };
            // Past the window, a required check that is still failing keeps
            // being sampled until it recovers or fails past settle: the
            // server judges it on the same settle clock (design §07), so the
            // agent must not stop just short of the rollback.
            if next > deadline && failing_since.is_empty() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(next) => {}
                _ = cancel.cancelled() => break,
            }
        }
    }

    /// Rolls `modules` of the applied revision back to the cached stable
    /// revision (planned with a module selector, applied through the runner)
    /// and reports it as a durable LocalRollback naming the modules.
    async fn module_rollback(
        &self,
        applied: &RevisionId,
        modules: Vec<String>,
        evidence: Vec<HealthResult>,
        settle: Duration,
    ) {
        let checks: Vec<String> = evidence
            .iter()
            .map(|r| format!("{}/{}", r.module, r.check))
            .collect();
        let reason = format!(
            "required checks failing for longer than {} during bake: {}",
            humantime_serde::re::humantime::format_duration(settle),
            checks.join(", ")
        );
        let stable = self.data.lock().await.agent.stable_revision.clone();
        let Some(stable) = stable.filter(|s| s != applied && self.store.has_revision(s)) else {
            tracing::warn!(modules = ?modules, %reason, "a module failed its bake but no cached stable revision differs from the applied one; leaving it to the server");
            return;
        };
        tracing::warn!(modules = ?modules, stable = %stable.short(), %reason, "rolling failed modules back to the stable revision");
        let selector = module_selector(&modules);
        let result = async {
            let rev = self.ensure_revision(&stable, None).await?;
            let mut ctx = self
                .context(&stable, Some(selector.clone()), Vec::new(), true)
                .await;
            ctx.secrets = self.rollback_secrets(&stable, applied).await;
            ctx.stable_revision = None;
            let (plan, _) = self.runner.plan(rev.clone(), &ctx).await?;
            Ok::<_, buckle_core::Error>(self.run_apply(rev, &ctx, &plan, false, false).await)
        }
        .await;
        let (succeeded, rollback_error, rollback_outcome) = match result {
            Ok(outcome) => (
                outcome.succeeded,
                (!outcome.succeeded).then(|| {
                    outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "a module failed".into())
                }),
                serde_json::to_value(&outcome).ok(),
            ),
            Err(err) => (
                false,
                Some(format!("the rollback could not run: {}", err.description())),
                None,
            ),
        };
        {
            let mut data = self.data.lock().await;
            for m in &modules {
                if !data.agent.rolled_back_modules.contains(m) {
                    data.agent.rolled_back_modules.push(m.clone());
                }
            }
            self.save_agent(&data);
        }
        let report_id = self.next_report_id().await;
        self.send_durable(Report::LocalRollback(LocalRollbackRecord {
            report_id,
            from: applied.clone(),
            to: stable,
            reason,
            modules,
            evidence,
            at: chrono::Utc::now(),
            succeeded,
            rollback_error,
            rollback_outcome,
        }))
        .await;
    }

    /// The secrets to render the stable revision with when rolling back: the
    /// union of the stable revision's and the current revision's (a secret
    /// the stable revision needs may only have been delivered with a later
    /// one). Where both hold a name, the current value wins: it is the one
    /// the host's services were last given, and a rotated-away value may no
    /// longer be valid.
    async fn rollback_secrets(
        &self,
        stable: &RevisionId,
        current: &RevisionId,
    ) -> Arc<SecretStore> {
        let mut merged = (*self.secrets_for(stable).await).clone();
        for secret in self.secrets_for(current).await.iter() {
            merged.insert(secret.clone());
        }
        Arc::new(merged)
    }

    /// Revisions whose trees and secrets must stay cached.
    fn keep_list(&self, data: &Data) -> Vec<RevisionId> {
        let mut keep: Vec<RevisionId> = [
            data.agent.applied_revision.clone(),
            data.agent.stable_revision.clone(),
            data.assigned.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        // The watchdog's fallback (and the revision it watches) must survive
        // a change of stable revision while it is armed.
        if let Some(w) = &data.agent.watchdog {
            keep.push(w.to.clone());
            keep.push(w.from.clone());
        }
        // Revisions of the cluster targets this host executes for.
        keep.extend(data.assignments.revisions().cloned());
        keep
    }

    /// Health reasons may echo secret values (a URL with a token, command
    /// output); scrub them before they go anywhere.
    async fn scrub_report(&self, revision: &RevisionId, mut report: HealthReport) -> HealthReport {
        let secrets = self.secrets_for(revision).await;
        if secrets.is_empty() {
            return report;
        }
        let redactor = buckle_core::secrets::redact::Redactor::new(&secrets);
        for r in &mut report.results {
            r.status = match std::mem::replace(&mut r.status, HealthStatus::Pass) {
                HealthStatus::Fail { reason } => HealthStatus::Fail {
                    reason: redactor.redact(&reason).into_inner(),
                },
                HealthStatus::Unknown { reason } => HealthStatus::Unknown {
                    reason: redactor.redact(&reason).into_inner(),
                },
                HealthStatus::Pass => HealthStatus::Pass,
            };
        }
        report
    }

    async fn drift(&self) {
        let (applied, paused, rolled_back) = {
            let data = self.data.lock().await;
            (
                data.agent.applied_revision.clone(),
                data.agent.status.as_deref() == Some("paused"),
                data.agent.rolled_back_modules.clone(),
            )
        };
        let Some(revision) = applied else { return };
        tracing::Span::current().record("buckle.revision", revision.0.as_str());
        let rev = match self.ensure_revision(&revision, None).await {
            Ok(rev) => rev,
            Err(err) => {
                tracing::warn!(error = %err.description(), "could not load the applied revision for the drift pass");
                return;
            }
        };
        // Drift mode for the plan and its correction alike: uncertain
        // selections are held (plan.held), and removals never run.
        let mut ctx = self.context(&revision, None, Vec::new(), false).await;
        ctx.drift = true;
        match self.runner.plan(rev.clone(), &ctx).await {
            Ok((plan, facts)) => {
                self.remember_facts(facts).await;
                if !plan.held.is_empty() {
                    tracing::warn!(held = ?plan.held.iter().map(|h| h.module.as_str()).collect::<Vec<_>>(), "drift held modules whose selection is uncertain");
                }
                let drifted = !plan.is_empty();
                let mut corrected = false;
                if drifted {
                    tracing::warn!(
                        changes = plan.changes(),
                        removals = plan.removals.len(),
                        "drift detected"
                    );
                    if self.config.drift.correct && !rolled_back.is_empty() {
                        tracing::warn!(modules = ?rolled_back, "not correcting drift: modules were rolled back after a failure; waiting for the server");
                    }
                    if self.config.drift.correct && !paused && rolled_back.is_empty() {
                        let outcome = self.run_apply(rev.clone(), &ctx, &plan, false, true).await;
                        if let Some(reboot) = &outcome.reboot {
                            // An unattended pass never reboots the host.
                            tracing::warn!(by = ?reboot.requested_by, "the drift correction asked for a reboot; not rebooting on its own: reboot the host or apply from the server");
                        }
                        corrected = outcome.succeeded;
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
        let report = self.scrub_report(&revision, report).await;
        self.sink
            .send(Report::Health {
                request_id: None,
                revision: Some(revision),
                report,
            })
            .await;
    }

    async fn facts(&self, force_report: bool) {
        let previous = self.data.lock().await.agent.facts.clone();
        match self.runner.facts(&previous).await {
            Ok(facts) => self.remember_facts_and_report(facts, force_report).await,
            Err(err) => tracing::warn!(error = %err.description(), "could not collect facts"),
        }
    }

    /// Before planning or applying `revision`, fetches the secrets that the
    /// modules selected for this host reference but the host does not hold.
    ///
    /// The server scopes a Welcome's or Assign's bundle with the facts it has
    /// for the host; on a first session, or when facts are stale there, a
    /// module gated on `host.facts.*` may be left out of the bundle while the
    /// agent, which has the real facts, selects it. The agent knows exactly
    /// which secrets its selection needs, so it asks for the gap (one call,
    /// only when something is missing) rather than failing the plan with an
    /// undefined secret. Failures are logged and the run goes ahead: the
    /// template error then names the missing secret.
    async fn ensure_secrets(&self, revision: &RevisionId, rev: &Revision) {
        if rev.fleet.secrets.is_empty() || !self.connected.load(Ordering::SeqCst) {
            return;
        }
        let ctx = {
            let data = self.data.lock().await;
            let host = buckle_core::selectors::HostIdentity {
                id: self.host_id.clone(),
                labels: buckle_core::engine::merged_labels(
                    &labels_json(&self.config.labels),
                    &labels_json(&data.agent.labels),
                ),
            };
            buckle_core::engine::host_context(rev, host, &data.agent.facts.tree)
        };
        let held = self.secrets_for(revision).await;
        let missing: Vec<String> = buckle_core::engine::secret_names_for_host(rev, &ctx)
            .into_iter()
            .filter(|name| !held.contains(name))
            .collect();
        if missing.is_empty() {
            return;
        }
        tracing::info!(revision = %revision.short(), count = missing.len(), "fetching secrets the selected modules need but this host does not hold");
        match self.transport.fetch_secrets(revision, &missing).await {
            Ok(wire) if !wire.is_empty() => {
                let mut data = self.data.lock().await;
                self.cache_secrets(&mut data, revision, &wire);
            }
            Ok(_) => {
                tracing::warn!(revision = %revision.short(), ?missing, "the server did not supply the missing secrets; they are not scoped to this host there")
            }
            Err(err) => {
                tracing::warn!(error = %err.description(), "could not fetch the missing secrets")
            }
        }
    }

    /// After facts change, more modules may select this host: ask the server
    /// for any declared secrets the host does not hold yet (it only returns
    /// those the newly selected modules reference).
    async fn top_up_secrets(&self) {
        if !self.connected.load(Ordering::SeqCst) {
            return;
        }
        // The assigned and applied revisions, and the stable one (a local or
        // module rollback renders it with the host's current selection).
        let ids: Vec<RevisionId> = {
            let data = self.data.lock().await;
            let mut ids: Vec<RevisionId> = [
                data.agent.applied_revision.clone(),
                data.assigned.clone(),
                data.agent.stable_revision.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let mut candidates: Vec<(RevisionId, Arc<Revision>)> = Vec::new();
        for id in ids {
            if !self.store.has_revision(&id) && !self.data.lock().await.revisions.contains_key(&id)
            {
                continue;
            }
            match self.ensure_revision(&id, None).await {
                Ok(rev) => candidates.push((id, rev)),
                Err(err) => {
                    tracing::debug!(revision = %id.short(), error = %err.description(), "cannot check a revision's secrets")
                }
            }
        }
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
                Err(err) => {
                    tracing::warn!(error = %err.description(), "could not fetch additional secrets")
                }
            }
        }
    }

    async fn upgrade(&self, version: String) {
        tracing::info!(%version, "upgrading the agent");
        let (outcome, restart) = match self.updater.update(Some(&version)).await {
            Ok(UpdateOutcome::Updated { version }) => {
                tracing::info!(%version, "upgraded; exiting so the service manager restarts the agent");
                (UpgradeReport::Updated, true)
            }
            Ok(UpdateOutcome::UpToDate { version }) => {
                tracing::info!(%version, "already running the requested version");
                (UpgradeReport::UpToDate, false)
            }
            Err(err) => {
                tracing::error!(error = %err.description(), "the upgrade failed");
                (UpgradeReport::Failed(err.description()), false)
            }
        };
        self.sink.send(Report::Upgrade { version, outcome }).await;
        if restart {
            self.stop.cancel();
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
                        .context(
                            &watchdog.state.from,
                            None,
                            watchdog.state.waived_checks.clone(),
                            false,
                        )
                        .await;
                    match self.runner.health(rev, &ctx, HealthScope::Bake, None).await {
                        Ok(report) => {
                            // Evidence ends up in agent.json and the report.
                            let report = self.scrub_report(&watchdog.state.from, report).await;
                            watchdog.observe(&report, Instant::now())
                        }
                        Err(err) => {
                            tracing::warn!(error = %err.description(), "the watchdog could not run health checks")
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err.description(), "the watchdog could not load the applied revision")
                }
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

    // ------------------------------------------------------------ reboots

    /// Remembers the reboot announced by durable report `report_id`, so that
    /// it is carried out once the server has stored the report, and survives
    /// a restart in between.
    async fn schedule_reboot(&self, report_id: u64, request: buckle_core::reboot::RebootRequest) {
        tracing::info!(by = ?request.requested_by, delay = ?request.delay, "the run asked for a reboot");
        let mut data = self.data.lock().await;
        data.agent.pending_reboot = Some(PendingReboot {
            report_id,
            request,
            requested_at: chrono::Utc::now(),
            issued_at: None,
        });
        self.save_agent(&data);
    }

    /// Carries out the pending reboot: waits for the server to acknowledge
    /// the report that announced it (at most `reboot_ack_grace`), then for
    /// its delay, then reboots and waits to be stopped. Nothing else runs in
    /// between. A reboot that cannot be started, or a host still up a
    /// `timeout` later, is reported as failed and forgotten.
    async fn reboot_if_pending(&self) {
        let Some(pending) = self.data.lock().await.agent.pending_reboot.clone() else {
            return;
        };
        if pending.issued_at.is_some() {
            // Started before, and the host is still on the same boot.
            self.reboot_failed("the host did not reboot after the reboot was started")
                .await;
            return;
        }
        let deadline = Instant::now() + self.options.reboot_ack_grace;
        loop {
            let acked = self.acked.notified();
            tokio::pin!(acked);
            acked.as_mut().enable();
            let pending_ack = self
                .data
                .lock()
                .await
                .agent
                .unacked_reports
                .iter()
                .any(|r| r.report_id() == pending.report_id);
            if !pending_ack {
                break;
            }
            tokio::select! {
                _ = acked => {}
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!("rebooting before the server acknowledged the result; it is sent again after the reboot");
                    break;
                }
                _ = self.stop.cancelled() => return,
            }
        }
        tokio::time::sleep(pending.request.delay).await;
        if self.stopping.load(Ordering::SeqCst) || self.stop.is_cancelled() {
            // Stopping anyway: the reboot is carried out on the next start.
            return;
        }
        {
            let mut data = self.data.lock().await;
            if let Some(p) = data.agent.pending_reboot.as_mut() {
                p.issued_at = Some(chrono::Utc::now());
            }
            self.save_agent(&data);
        }
        tracing::warn!(by = ?pending.request.requested_by, "rebooting the host");
        if let Err(err) = self.runner.reboot().await {
            self.reboot_failed(&err.description()).await;
            return;
        }
        let until = Instant::now() + pending.request.timeout;
        while Instant::now() < until {
            if self.stopping.load(Ordering::SeqCst) || self.stop.is_cancelled() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250).min(pending.request.timeout)).await;
        }
        self.reboot_failed(&format!(
            "the host was still up {} after the reboot was started",
            humantime_serde::re::humantime::format_duration(pending.request.timeout)
        ))
        .await;
    }

    async fn reboot_failed(&self, error: &str) {
        tracing::error!(%error, "the reboot the run asked for did not happen");
        {
            let mut data = self.data.lock().await;
            data.agent.pending_reboot = None;
            self.save_agent(&data);
        }
        self.sink
            .send(Report::RebootFailed {
                boot_id: self.current_boot(),
                error: error.to_owned(),
            })
            .await;
    }

    async fn local_rollback(&self, watchdog: Watchdog, reason: String) {
        let from = watchdog.state.from.clone();
        let to = watchdog.state.to.clone();
        tracing::warn!(from = %from.short(), to = %to.short(), %reason, "rolling back to the cached stable revision");
        let result = async {
            let rev = self.ensure_revision(&to, None).await?;
            let mut ctx = self.context(&to, None, Vec::new(), true).await;
            ctx.secrets = self.rollback_secrets(&to, &from).await;
            let (plan, facts) = self.runner.plan(rev.clone(), &ctx).await?;
            self.remember_facts(facts).await;
            Ok::<_, buckle_core::Error>(self.run_apply(rev, &ctx, &plan, false, true).await)
        }
        .await;
        let reboot = result.as_ref().ok().and_then(|o| o.reboot.clone());
        let (succeeded, rollback_error, rollback_outcome) = match result {
            Ok(outcome) => (
                outcome.succeeded,
                (!outcome.succeeded).then(|| {
                    outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "a module failed".into())
                }),
                serde_json::to_value(&outcome).ok(),
            ),
            Err(err) => (
                false,
                Some(format!("the rollback could not run: {}", err.description())),
                None,
            ),
        };
        if !succeeded {
            tracing::error!(%reason, error = ?rollback_error, "the local rollback failed; the host needs attention");
        }
        let report_id = self.next_report_id().await;
        if let Some(reboot) = reboot {
            // Restoring the stable revision asked for a reboot (boot
            // configuration, say): it follows the report like an apply's.
            self.schedule_reboot(report_id, reboot).await;
        }
        let record = LocalRollbackRecord {
            report_id,
            from,
            to,
            reason,
            modules: Vec::new(),
            evidence: watchdog.evidence().to_vec(),
            at: chrono::Utc::now(),
            succeeded,
            rollback_error,
            rollback_outcome,
        };
        // Kept until the server acknowledges it; sent now if a session is up,
        // otherwise after the next Welcome.
        self.send_durable(Report::LocalRollback(record)).await;
    }
}

fn op_for(instruction: Instruction) -> Option<Op> {
    Some(match instruction {
        Instruction::Assign {
            revision,
            stable,
            secrets,
            tree_digest,
            target: _,
            workload: _,
            cluster: _,
        } => Op::Assign {
            revision,
            stable,
            secrets,
            tree_digest,
            desired: true,
        },
        Instruction::RequestPlan {
            revision,
            select,
            target: _,
            workload: _,
        } => Op::Plan { revision, select },
        Instruction::Apply {
            revision,
            plan_digest,
            approval_digest,
            waived_checks,
            force,
            traceparent,
            select,
            rollout_id,
            phase,
            target: _,
            workload: _,
        } => Op::Apply {
            revision,
            plan_digest,
            approval_digest,
            waived_checks,
            force,
            traceparent,
            select,
            rollout_id,
            phase,
        },
        Instruction::Rollback {
            revision,
            reason,
            rollout_id,
            traceparent,
            target: _,
            workload: _,
        } => Op::Rollback {
            revision,
            reason,
            rollout_id,
            traceparent,
        },
        Instruction::RunHealth {
            request_id,
            scope,
            modules,
            until,
            traceparent,
            target: _,
            workload: _,
            revision: _,
        } => Op::Health {
            request_id,
            scope,
            modules,
            until,
            traceparent,
        },
        Instruction::RunAction {
            action_id,
            module,
            action,
            revision,
            traceparent,
            target: _,
            workload: _,
        } => Op::Action {
            action_id,
            module,
            action,
            revision,
            traceparent,
        },
        Instruction::Upgrade { version, when_idle } => Op::Upgrade { version, when_idle },
        Instruction::Welcome(_)
        | Instruction::Ping
        | Instruction::Ack { .. }
        | Instruction::HostUpdate(_)
        | Instruction::Rejected { .. } => return None,
    })
}
