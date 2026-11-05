//! Lanes: one task per cluster target this agent speaks for, beside the
//! host's own worker (design: workload-centric rollouts, §07).
//!
//! The host lane drains the session's queue one operation at a time; a long
//! cluster apply there would block the host's drift pass and health checks,
//! and a host apply would block cluster work. So every cluster gets a lane
//! of its own: its own queue, credential (through the executor's cache),
//! watch and concurrency budget (the executor's per-plan cap). Lanes share
//! the session, the report sink and the revision store.
//!
//! A lane opens with the first targeted instruction for its cluster (an
//! older server sends those without a lease) or with a [`Instruction::Lease`],
//! which also starts the **watch**: health read when the cluster changes
//! (Nomad blocking queries, a Kubernetes watch stream) and every
//! [`WatchConfig::health`] regardless, the applied revision re-planned for
//! drift every [`WatchConfig::drift`], and the cluster's reachability
//! reported as it changes. A [`Instruction::Release`] (or the agent's
//! shutdown) ends the lane once the instruction it is running finishes.

use super::{Report, Shared};
use crate::cluster::{Assignment, TargetSpec};
use crate::transport::{Instruction, WatchConfig, WireCluster};
use buckle_core::health::HealthScope;
use buckle_core::plan::RevisionId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// How long a Nomad blocking query waits for a change (under the client's
/// request timeout, with room for Nomad's jitter).
const BLOCKING_WAIT: Duration = Duration::from_secs(50);
/// How long the lane waits after a change (or an instruction) before
/// reading health, so a burst of events is read once.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a watcher waits before retrying after the cluster did not answer.
const RETRY: Duration = Duration::from_secs(15);
/// How often a Kubernetes watch re-lists when the credential never expires
/// (`local`, `workload`, `secret://`): the identity TTL it mirrors.
const RESYNC: Duration = Duration::from_secs(15 * 60);

/// What a lane is asked to do.
pub(super) enum LaneOp {
    /// A targeted instruction (Assign, RequestPlan, Apply, Rollback,
    /// RunHealth, RunAction); `generation` is the session that asked, for
    /// instructions that are skipped once it ended.
    Instruction {
        instruction: Instruction,
        generation: Option<u64>,
    },
    /// The server leased (or re-leased) the cluster to this agent.
    Lease(LeaseTerms),
    /// The server took the lease away.
    Release { reason: String },
}

/// What a lease says.
pub(super) struct LeaseTerms {
    pub cluster: Option<WireCluster>,
    pub applied: Option<RevisionId>,
    pub watch: WatchConfig,
}

struct LaneHandle {
    tx: mpsc::UnboundedSender<LaneOp>,
    busy: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

/// The lanes of the cluster targets this agent speaks for, by cluster.
#[derive(Default)]
pub(super) struct Lanes {
    lanes: Mutex<HashMap<String, LaneHandle>>,
}

impl std::fmt::Debug for Lanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self
            .lanes
            .lock()
            .map(|l| l.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("Lanes").field("clusters", &names).finish()
    }
}

impl Lanes {
    /// Queues `op` on `target`'s lane, opening the lane when there is none
    /// (a Release for a cluster without a lane does nothing).
    pub(super) fn dispatch(&self, shared: &Arc<Shared>, target: &str, op: LaneOp) {
        let Ok(mut lanes) = self.lanes.lock() else {
            return;
        };
        // A lane that ended (released) is replaced by the next instruction.
        if lanes.get(target).is_some_and(|l| l.task.is_finished()) {
            lanes.remove(target);
        }
        if !lanes.contains_key(target) {
            if matches!(op, LaneOp::Release { .. }) {
                return;
            }
            tracing::info!(%target, "opening a lane for the cluster");
            lanes.insert(target.to_owned(), Lane::open(shared.clone(), target));
        }
        let lane = &lanes[target];
        if matches!(op, LaneOp::Instruction { .. }) {
            lane.queued.fetch_add(1, Ordering::SeqCst);
        }
        if lane.tx.send(op).is_err() {
            lane.queued.store(0, Ordering::SeqCst);
        }
    }

    /// Whether any lane is running or holds an instruction.
    pub(super) fn busy(&self) -> bool {
        self.lanes
            .lock()
            .map(|l| {
                l.values().any(|lane| {
                    lane.busy.load(Ordering::SeqCst) || lane.queued.load(Ordering::SeqCst) > 0
                })
            })
            .unwrap_or(false)
    }

    /// Closes every lane's queue (each ends after the instruction it is
    /// running) and returns their tasks to wait for.
    pub(super) fn close(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let Ok(mut lanes) = self.lanes.lock() else {
            return Vec::new();
        };
        lanes.drain().map(|(_, l)| l.task).collect()
    }
}

/// What wakes a lane's watch.
enum WatchEvent {
    /// The cluster changed (an index moved, an object event arrived).
    Changed,
    /// The cluster did not answer (or refused the credential).
    Unreachable(String),
    /// It answers again.
    Reachable,
}

/// A running watcher task.
struct Watcher {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    /// The revision it watches, to restart it when the applied one moves.
    revision: Option<RevisionId>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

/// The watch a lease starts.
struct Watch {
    spec: Arc<TargetSpec>,
    applied: Option<RevisionId>,
    config: WatchConfig,
    events: mpsc::Receiver<WatchEvent>,
    events_tx: mpsc::Sender<WatchEvent>,
    watcher: Option<Watcher>,
    /// What was last reported; `None` before the first probe.
    reachable: Option<bool>,
    health_due: Instant,
    drift_due: Instant,
}

/// One cluster's lane.
struct Lane {
    shared: Arc<Shared>,
    target: String,
    busy: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    watch: Option<Watch>,
}

impl Lane {
    fn open(shared: Arc<Shared>, target: &str) -> LaneHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let busy = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let lane = Lane {
            shared,
            target: target.to_owned(),
            busy: busy.clone(),
            queued: queued.clone(),
            watch: None,
        };
        let span = tracing::info_span!("agent.lane", buckle.target = %target);
        let task = tokio::spawn(lane.run(rx).instrument(span));
        LaneHandle {
            tx,
            busy,
            queued,
            task,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<LaneOp>) {
        loop {
            let watching = self.watch.is_some();
            let health_due = self
                .watch
                .as_ref()
                .map(|w| w.health_due)
                .unwrap_or_else(Instant::now);
            let drift_due = self
                .watch
                .as_ref()
                .map(|w| w.drift_due)
                .unwrap_or_else(Instant::now);
            tokio::select! {
                op = rx.recv() => match op {
                    None => break,
                    Some(LaneOp::Release { reason }) => {
                        tracing::info!(target = %self.target, %reason, "the lease ended; closing the lane");
                        break;
                    }
                    Some(LaneOp::Lease(terms)) => self.lease(terms).await,
                    Some(LaneOp::Instruction { instruction, generation }) => {
                        self.queued.fetch_sub(1, Ordering::SeqCst);
                        self.instruction(instruction, generation).await;
                    }
                },
                event = async {
                    match self.watch.as_mut() {
                        Some(w) => w.events.recv().await,
                        None => None,
                    }
                }, if watching => match event {
                    Some(WatchEvent::Changed) => {
                        if let Some(w) = &mut self.watch {
                            w.health_due = w.health_due.min(Instant::now() + SETTLE);
                        }
                    }
                    Some(WatchEvent::Unreachable(error)) => self.set_reachable(false, Some(error)).await,
                    Some(WatchEvent::Reachable) => self.set_reachable(true, None).await,
                    // The watcher ended on its own; the timers still read.
                    None => {}
                },
                _ = tokio::time::sleep_until(health_due), if watching => self.read_health().await,
                _ = tokio::time::sleep_until(drift_due), if watching => self.read_drift().await,
            }
        }
        self.watch = None;
    }

    /// Runs a targeted instruction, unless its session ended first or the
    /// agent is stopping.
    async fn instruction(&mut self, instruction: Instruction, generation: Option<u64>) {
        if self.shared.stopping.load(Ordering::SeqCst) || self.shared.stop.is_cancelled() {
            tracing::info!(target = %self.target, "not starting a cluster operation: the agent is stopping");
            return;
        }
        if let Some(generation) = generation
            && generation != self.shared.generation.load(Ordering::SeqCst)
        {
            tracing::debug!(target = %self.target, instruction = ?std::mem::discriminant(&instruction), "skipping a cluster operation queued by a session that has ended");
            return;
        }
        let changes = !matches!(instruction, Instruction::Assign { .. });
        self.busy.store(true, Ordering::SeqCst);
        let applied = self
            .shared
            .target_op(self.target.clone(), instruction)
            .await;
        self.busy.store(false, Ordering::SeqCst);
        let moved = match (&mut self.watch, applied) {
            (Some(w), Some(revision)) if w.applied.as_ref() != Some(&revision) => {
                w.applied = Some(revision);
                true
            }
            _ => false,
        };
        if moved {
            self.restart_watcher().await;
        }
        if let Some(w) = &mut self.watch
            && changes
        {
            // Read what the operation left behind once it settles, and plan
            // for drift only after a quiet spell.
            let now = Instant::now();
            w.health_due = now + SETTLE;
            w.drift_due = now + w.config.drift;
        }
    }

    /// Takes (or refreshes) the lease: the cluster's spec, applied
    /// revision and watch intervals, and a running watcher.
    async fn lease(&mut self, terms: LeaseTerms) {
        let spec = match terms.cluster.as_ref().map(super::targets::target_spec) {
            Some(Ok(spec)) => Some(Arc::new(spec)),
            Some(Err(err)) => {
                tracing::error!(target = %self.target, error = %err.description(), "the lease describes the cluster in a way this agent cannot use");
                return;
            }
            None => None,
        };
        let now = Instant::now();
        match &mut self.watch {
            Some(w) => {
                let restart = w.applied != terms.applied
                    || spec.as_ref().is_some_and(|s| {
                        s.identity != w.spec.identity
                            || s.api != w.spec.api
                            || s.kind != w.spec.kind
                    });
                if let Some(spec) = spec {
                    w.spec = spec;
                }
                w.applied = terms.applied;
                w.config = terms.watch;
                if restart {
                    tracing::info!(target = %self.target, applied = ?w.applied.as_ref().map(|r| r.short()), "the lease changed; watching afresh");
                    w.health_due = now;
                    w.drift_due = now + w.config.drift;
                    self.restart_watcher().await;
                }
            }
            None => {
                let Some(spec) = spec else {
                    tracing::error!(target = %self.target, "the lease does not describe the cluster; not watching it");
                    return;
                };
                tracing::info!(target = %self.target, applied = ?terms.applied.as_ref().map(|r| r.short()), health = ?terms.watch.health, drift = ?terms.watch.drift, "leased the cluster; watching it");
                let (events_tx, events) = mpsc::channel(64);
                self.watch = Some(Watch {
                    spec,
                    applied: terms.applied,
                    config: terms.watch,
                    events,
                    events_tx,
                    watcher: None,
                    reachable: None,
                    health_due: now,
                    drift_due: now + terms.watch.drift,
                });
                self.restart_watcher().await;
            }
        }
    }

    /// The assignment of the applied revision, which the watch reads with.
    async fn applied_assignment(&self) -> Option<Assignment> {
        let w = self.watch.as_ref()?;
        let revision = w.applied.as_ref()?;
        let data = self.shared.data.lock().await;
        match data.assignments.get(&self.target, revision) {
            Some(a) => Some(a.clone()),
            None => {
                tracing::warn!(target = %self.target, revision = %revision.short(), "the applied revision was not assigned to this agent; the server re-sends it with the lease");
                None
            }
        }
    }

    /// (Re)starts the watcher for the applied revision: the task that long
    /// polls Nomad or streams Kubernetes events and wakes the lane.
    async fn restart_watcher(&mut self) {
        let Some(a) = self.applied_assignment().await else {
            if let Some(w) = &mut self.watch {
                w.watcher = None;
            }
            return;
        };
        let Some(w) = &mut self.watch else { return };
        if w.watcher
            .as_ref()
            .is_some_and(|r| r.revision.as_ref() == Some(&a.revision.id))
        {
            return;
        }
        let cancel = CancellationToken::new();
        let shared = self.shared.clone();
        let tx = w.events_tx.clone();
        let kind = a.spec.kind;
        let span = tracing::info_span!("agent.watch", buckle.target = %self.target, kind = ?kind);
        let task = tokio::spawn(
            {
                let cancel = cancel.clone();
                async move {
                    match kind {
                        buckle_core::model::ClusterKind::Nomad => {
                            watch_nomad(shared, a, tx, cancel).await
                        }
                        buckle_core::model::ClusterKind::Kubernetes => {
                            watch_kubernetes(shared, a, tx, cancel).await
                        }
                    }
                }
            }
            .instrument(span),
        );
        w.watcher = Some(Watcher {
            cancel,
            task,
            revision: w.applied.clone(),
        });
    }

    /// Reads the cluster's health (every applied workload's implicit
    /// checks) and reports it as a drift-scope health report, as a host
    /// agent's drift pass does; with nothing applied, probes the API.
    async fn read_health(&mut self) {
        let Some(w) = &mut self.watch else { return };
        w.health_due = Instant::now() + w.config.health;
        match self.applied_assignment().await {
            Some(a) => {
                let span = tracing::info_span!(parent: None, "agent.health", buckle.host = %self.shared.host_id, buckle.target = %self.target, buckle.revision = %a.revision.id.0, scope = ?HealthScope::Drift);
                self.shared.follow_session(&span);
                let read = self
                    .shared
                    .executor
                    .health(&a, HealthScope::Drift, None, None)
                    .instrument(span)
                    .await;
                match read {
                    Ok(report) => {
                        self.set_reachable(true, None).await;
                        self.shared
                            .sink
                            .send(
                                Report::Health {
                                    request_id: None,
                                    revision: Some(a.revision.id.clone()),
                                    report,
                                }
                                .about(Some(&self.target)),
                            )
                            .await;
                    }
                    Err(err) => {
                        tracing::warn!(target = %self.target, error = %err.description(), "the cluster's health could not be read");
                        self.set_reachable(false, Some(err.description())).await;
                    }
                }
            }
            None => {
                let (target, spec) = match &self.watch {
                    Some(w) => (self.target.clone(), w.spec.clone()),
                    None => return,
                };
                match self.shared.executor.probe(&target, &spec).await {
                    Ok(()) => self.set_reachable(true, None).await,
                    Err(err) => self.set_reachable(false, Some(err.description())).await,
                }
            }
        }
    }

    /// Plans the applied revision against the cluster and reports what
    /// differs (report-only: nothing is corrected).
    async fn read_drift(&mut self) {
        let Some(w) = &mut self.watch else { return };
        w.drift_due = Instant::now() + w.config.drift;
        let Some(a) = self.applied_assignment().await else {
            return;
        };
        let span = tracing::info_span!(parent: None, "agent.drift", buckle.host = %self.shared.host_id, buckle.target = %self.target, buckle.revision = %a.revision.id.0, buckle.plan.digest = tracing::field::Empty);
        self.shared.follow_session(&span);
        match self
            .shared
            .executor
            .plan(&a, None, None)
            .instrument(span.clone())
            .await
        {
            Ok(planned) => {
                let plan = planned.plan;
                span.record("buckle.plan.digest", plan.digest.to_string().as_str());
                let drifted = !plan.is_empty();
                if drifted {
                    tracing::warn!(target = %self.target, changes = plan.changes(), removals = plan.removals.len(), "cluster drift detected");
                }
                self.set_reachable(true, None).await;
                self.shared
                    .sink
                    .send(
                        Report::Drift {
                            revision: a.revision.id.clone(),
                            plan: drifted.then_some(plan),
                            corrected: false,
                        }
                        .about(Some(&self.target)),
                    )
                    .await;
            }
            Err(err) => {
                tracing::warn!(target = %self.target, error = %err.description(), "the cluster's drift pass could not plan");
                self.set_reachable(false, Some(err.description())).await;
            }
        }
    }

    /// Records whether the cluster answers and reports a change.
    async fn set_reachable(&mut self, reachable: bool, error: Option<String>) {
        let Some(w) = &mut self.watch else { return };
        if w.reachable == Some(reachable) {
            return;
        }
        w.reachable = Some(reachable);
        if reachable {
            tracing::info!(target = %self.target, "the cluster answers");
        } else {
            tracing::warn!(target = %self.target, error = error.as_deref().unwrap_or(""), "the cluster does not answer");
        }
        self.shared
            .sink
            .send(Report::TargetStatus { reachable, error }.about(Some(&self.target)))
            .await;
    }
}

/// Long polls Nomad's jobs, allocations and deployments (every namespace)
/// and wakes the lane whenever one of their indexes moves. A dropped poll
/// is covered by the lane's health timer.
async fn watch_nomad(
    shared: Arc<Shared>,
    a: Assignment,
    tx: mpsc::Sender<WatchEvent>,
    cancel: CancellationToken,
) {
    let mut polls = tokio::task::JoinSet::new();
    for path in [
        "/v1/jobs?namespace=*",
        "/v1/allocations?namespace=*",
        "/v1/deployments?namespace=*",
    ] {
        let shared = shared.clone();
        let a = a.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        polls.spawn(async move {
            let mut index = 0u64;
            let mut failed = false;
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let read = async {
                    let nomad = shared.executor.read_nomad(&a).await?;
                    nomad
                        .get_blocking::<serde_json::Value>(path, index, BLOCKING_WAIT)
                        .await
                };
                let read = tokio::select! {
                    r = read => r,
                    _ = cancel.cancelled() => return,
                };
                match read {
                    Ok(answer) => {
                        if failed {
                            failed = false;
                            let _ = tx.send(WatchEvent::Reachable).await;
                        }
                        let Some(answer) = answer else {
                            // Nothing served at this path (a 404 answers at
                            // once, not after `wait`): the health timer
                            // covers it; look again later.
                            tokio::select! {
                                _ = tokio::time::sleep(BLOCKING_WAIT) => continue,
                                _ = cancel.cancelled() => return,
                            }
                        };
                        if answer.index > index || index == 0 {
                            index = answer.index.max(1);
                            if tx.send(WatchEvent::Changed).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        failed = true;
                        if tx
                            .send(WatchEvent::Unreachable(err.description()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(RETRY) => {}
                            _ = cancel.cancelled() => return,
                        }
                    }
                }
            }
        });
    }
    tokio::select! {
        _ = cancel.cancelled() => polls.abort_all(),
        _ = async { while polls.join_next().await.is_some() {} } => {}
    }
}

/// Streams Kubernetes watch events for every kind the cluster's index lists
/// for the target and wakes the lane on each. The stream is trusted on its
/// own; the full re-sync (a fresh list, which the lane reads as a health
/// pass) happens when the credential is re-issued, every identity TTL.
async fn watch_kubernetes(
    shared: Arc<Shared>,
    a: Assignment,
    tx: mpsc::Sender<WatchEvent>,
    cancel: CancellationToken,
) {
    use futures::StreamExt;
    let mut failed = false;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let opened = async {
            let (kube, expires) = shared.executor.read_kube(&a).await?;
            let index = buckle_kube::index::read(&kube, &a.target).await?;
            let mut kinds: Vec<(String, String)> = index
                .entries
                .iter()
                .map(|e| (e.key.api_version.clone(), e.key.kind.clone()))
                .collect();
            kinds.sort();
            kinds.dedup();
            let selector = format!(
                "{}={}",
                buckle_kube::object::TARGET_LABEL,
                buckle_kube::object::label_value(&a.target)
            );
            let mut streams = Vec::new();
            for (api_version, kind) in kinds {
                let Some(resource) = kube.resource(&api_version, &kind).await? else {
                    continue;
                };
                let path = buckle_kube::client::with_query(
                    &resource.path("", None),
                    "labelSelector",
                    &selector,
                );
                streams.push(kube.watch(&path, "").await?);
            }
            Ok::<_, buckle_core::Error>((streams, expires))
        };
        let opened = tokio::select! {
            r = opened => r,
            _ = cancel.cancelled() => return,
        };
        let (streams, expires) = match opened {
            Ok(v) => v,
            Err(err) => {
                failed = true;
                if tx
                    .send(WatchEvent::Unreachable(err.description()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(RETRY) => {}
                    _ = cancel.cancelled() => return,
                }
                continue;
            }
        };
        if failed {
            failed = false;
            let _ = tx.send(WatchEvent::Reachable).await;
        }
        // A fresh list: read everything once.
        if tx.send(WatchEvent::Changed).await.is_err() {
            return;
        }
        let resync = expires
            .and_then(|at| {
                (at - chrono::Utc::now() - crate::identity::RENEW_BEFORE)
                    .to_std()
                    .ok()
            })
            .filter(|d| !d.is_zero())
            .unwrap_or(RESYNC);
        let deadline = tokio::time::sleep(resync);
        tokio::pin!(deadline);
        let mut events = futures::stream::select_all(streams);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = &mut deadline => {
                    tracing::debug!(target = %a.target, "re-syncing the Kubernetes watch with a fresh identity");
                    break;
                }
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        if event["type"].as_str() == Some("BOOKMARK") {
                            continue;
                        }
                        if event["type"].as_str() == Some("ERROR") {
                            tracing::debug!(target = %a.target, status = %event["object"], "the Kubernetes watch reported an error; reopening it");
                            break;
                        }
                        if tx.send(WatchEvent::Changed).await.is_err() {
                            return;
                        }
                    }
                    Some(Err(err)) => {
                        failed = true;
                        if tx.send(WatchEvent::Unreachable(err.description())).await.is_err() {
                            return;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(RETRY) => {}
                            _ = cancel.cancelled() => return,
                        }
                        break;
                    }
                    // The API server closed the watches (it does so on its
                    // own schedule) or none were open: reopen them.
                    None => {
                        if events.is_empty() {
                            tokio::select! {
                                _ = &mut deadline => {}
                                _ = cancel.cancelled() => return,
                            }
                        }
                        break;
                    }
                },
            }
        }
    }
}
