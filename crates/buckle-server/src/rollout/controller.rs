//! The controller actor. One task owns all live rollout state; every input
//! (agent message, connect, disconnect, operator command, timer) is handled
//! to completion, checkpointed to the store in one transaction with its
//! audit rows, broadcast as live events, and only then are the resulting
//! messages sent to hosts.

use super::link::HostLink;
use super::model::{
    GateRuntime, HostPhase, HostRuntime, PauseInfo, PhaseRuntime, RolloutHostRecord, RolloutRecord,
    RolloutRuntime, STRAGGLERS,
};
use super::view;
use crate::clock::Clock;
use crate::events::EventBus;
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{ApprovalRecord, NewAudit, Store, kv::keys};
use buckle_api::{HostStatus, LiveEvent, PhaseState, RevisionStatus, RolloutState};
use buckle_core::engine::Revision;
use buckle_core::model::{BaselinePolicy, FailurePolicy, SupersedePolicy};
use buckle_proto::v1::{
    self as pb, Apply, ApplyResult, Assign, HealthReport, HealthScope, LocalRollback, PlanResult,
    RequestPlan, RunHealth, ServerMessage, agent_message, server_message,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Span;

/// Tunables that are not part of the fleet's rollout policy.
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// How long an in-flight host may stay disconnected before it fails.
    pub disconnect_timeout: Duration,
    /// How long a host may stay applying without a result before it fails.
    pub apply_timeout: Duration,
}

/// How often an Apply is re-sent to a host that reconnects without having
/// run it before the host is failed instead.
const MAX_APPLY_RESENDS: u32 = 3;

/// The task selector of every RequestPlan (rollouts plan whole revisions),
/// echoed in the Apply so the agent re-plans exactly what was approved.
const PLAN_SELECT: &str = "";

impl Default for ControllerOptions {
    fn default() -> Self {
        Self {
            disconnect_timeout: Duration::from_secs(600),
            apply_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Everything the controller needs.
pub struct ControllerDeps {
    pub store: Store,
    pub link: Arc<dyn HostLink>,
    pub clock: Arc<dyn Clock>,
    pub events: EventBus,
    pub revisions: Arc<RevisionCache>,
    pub secrets: ServerSecrets,
    /// Key for approval tokens (the server install key).
    pub approval_key: [u8; 32],
    pub options: ControllerOptions,
}

/// A host that just completed its Hello.
#[derive(Debug, Clone, Default)]
pub struct Connected {
    pub host: String,
    pub applied_revision: Option<String>,
    pub last_plan_digest: Option<String>,
    /// Revisions whose Apply is running on the agent, or ran with a result
    /// the server has not acknowledged yet (it follows the Welcome).
    pub unreported_apply_revisions: Vec<String>,
    /// A new session (Hello). `false` when the controller re-evaluates an
    /// already connected host (e.g. it was resumed): its Hello facts are
    /// stale, so nothing is re-sent on their account.
    pub new_session: bool,
}

/// Operator actions (design §10 "Paused offers four operator decisions").
#[derive(Debug, Clone)]
pub enum Command {
    /// Start a rollout of a valid revision.
    Start {
        revision: String,
        note: String,
    },
    /// Approve a phase, binding plan digests.
    Approve {
        rollout: String,
        phase: String,
        digests: Vec<String>,
        note: String,
    },
    Pause {
        rollout: String,
        note: String,
    },
    Resume {
        rollout: String,
        note: String,
    },
    /// Apply the previous stable revision to every updated host, newest phase first.
    Rollback {
        rollout: String,
        note: String,
    },
    Abort {
        rollout: String,
        note: String,
    },
    /// Re-plan and re-apply a failed host.
    Retry {
        rollout: String,
        host: String,
        note: String,
    },
    /// Exclude a host from the rollout.
    Skip {
        rollout: String,
        host: String,
        note: String,
    },
    /// Retry a host with failed checks waived.
    Continue {
        rollout: String,
        host: String,
        waive: Vec<String>,
        note: String,
    },
    /// Apply a host held at baseline anyway.
    Force {
        rollout: String,
        host: String,
        note: String,
    },
    /// Turn down a rollout awaiting approval: it ends aborted with the
    /// reason (design §10 "AwaitingApproval → Aborted: rejected").
    Reject {
        rollout: String,
        reason: String,
    },
}

impl Command {
    fn action(&self) -> &'static str {
        match self {
            Command::Start { .. } => "rollout.start",
            Command::Approve { .. } => "rollout.approve",
            Command::Pause { .. } => "rollout.pause",
            Command::Resume { .. } => "rollout.resume",
            Command::Rollback { .. } => "rollout.rollback",
            Command::Abort { .. } => "rollout.abort",
            Command::Retry { .. } => "rollout.host.retry",
            Command::Skip { .. } => "rollout.host.skip",
            Command::Continue { .. } => "rollout.host.continue",
            Command::Force { .. } => "rollout.host.force",
            Command::Reject { .. } => "rollout.reject",
        }
    }
}

/// How a command failed, for the HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandErrorKind {
    NotFound,
    Conflict,
    Invalid,
    Internal,
}

/// A refused or failed command.
#[derive(Debug)]
pub struct CommandError {
    pub kind: CommandErrorKind,
    pub error: buckle_core::Error,
}

impl CommandError {
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            kind: CommandErrorKind::NotFound,
            error: buckle_core::error::user(
                msg.into(),
                &["Refresh the page; it may have changed."],
            ),
        }
    }
    fn conflict(msg: impl Into<String>, advice: &'static [&'static str]) -> Self {
        Self {
            kind: CommandErrorKind::Conflict,
            error: buckle_core::error::user(msg.into(), advice),
        }
    }
    fn invalid(msg: impl Into<String>, advice: &'static [&'static str]) -> Self {
        Self {
            kind: CommandErrorKind::Invalid,
            error: buckle_core::error::user(msg.into(), advice),
        }
    }
    fn internal(error: buckle_core::Error) -> Self {
        Self {
            kind: CommandErrorKind::Internal,
            error,
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

enum Input {
    Connected(Connected),
    Disconnected(String),
    Agent(String, agent_message::Msg),
    RevisionAdded(String),
    HostChanged(String),
    HostRegistered(String),
    /// A durable agent report (ApplyResult, LocalRollback) the gRPC layer
    /// stored as pending; `done` is told once the controller's reaction is
    /// persisted (and the pending copy removed), so the report can be acked.
    Durable {
        host: String,
        report_id: u64,
        msg: agent_message::Msg,
        done: Option<oneshot::Sender<bool>>,
    },
    Command {
        actor: String,
        command: Command,
        reply: oneshot::Sender<Result<String, CommandError>>,
    },
}

/// The way in: agents' messages, connection changes and operator commands.
#[derive(Clone)]
pub struct ControllerHandle {
    tx: mpsc::UnboundedSender<Input>,
}

impl std::fmt::Debug for ControllerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControllerHandle(..)")
    }
}

impl ControllerHandle {
    /// A host's session is established (its link is attached).
    pub fn connected(&self, c: Connected) {
        let _ = self.tx.send(Input::Connected(c));
    }

    /// A host's session ended.
    pub fn disconnected(&self, host: &str) {
        let _ = self.tx.send(Input::Disconnected(host.to_owned()));
    }

    /// A message from a host's agent.
    pub fn agent_message(&self, host: &str, msg: agent_message::Msg) {
        let _ = self.tx.send(Input::Agent(host.to_owned(), msg));
    }

    /// The source watcher stored a new revision.
    pub fn revision_added(&self, id: &str) {
        let _ = self.tx.send(Input::RevisionAdded(id.to_owned()));
    }

    /// An operator changed a host's status, labels, or forgot it.
    pub fn host_changed(&self, host: &str) {
        let _ = self.tx.send(Input::HostChanged(host.to_owned()));
    }

    /// A durable agent report the gRPC layer stored as pending (see
    /// [`pending_report_key`]). The returned receiver resolves to `true` once
    /// the controller has persisted its reaction and removed the pending
    /// copy: only then may the report be acknowledged to the agent. It
    /// resolves to `false` (or is dropped) when the controller stops first.
    pub fn durable_report(
        &self,
        host: &str,
        report_id: u64,
        msg: agent_message::Msg,
    ) -> oneshot::Receiver<bool> {
        let (done, rx) = oneshot::channel();
        let _ = self.tx.send(Input::Durable {
            host: host.to_owned(),
            report_id,
            msg,
            done: Some(done),
        });
        rx
    }

    /// A host row was created by registration or enrolment (possibly a
    /// forgotten host coming back): refresh whether it is excluded. Its
    /// session's Welcome tells the agent its status, so nothing is pushed.
    pub fn host_registered(&self, host: &str) {
        let _ = self.tx.send(Input::HostRegistered(host.to_owned()));
    }

    /// Runs an operator command and returns the rollout id it concerned.
    pub async fn command(&self, actor: &str, command: Command) -> Result<String, CommandError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Input::Command {
                actor: actor.to_owned(),
                command,
                reply,
            })
            .map_err(|_| CommandError::internal(stopped()))?;
        rx.await.map_err(|_| CommandError::internal(stopped()))?
    }
}

fn not_saved() -> buckle_core::Error {
    buckle_core::error::system(
        "The rollout controller accepted the change but could not save it yet.",
        &[
            "The controller keeps retrying and the change takes effect once it is saved; check the server logs for the database error.",
        ],
    )
}

fn stopped() -> buckle_core::Error {
    buckle_core::error::system(
        "The rollout controller is not running.",
        &["The server is shutting down or the controller crashed; check the server logs."],
    )
}

/// The kv key under which the gRPC layer keeps a durable report until the
/// controller has persisted its reaction to it.
pub fn pending_report_key(host: &str, report_id: u64) -> String {
    format!("{}{host}/{report_id}", keys::PENDING_REPORT_PREFIX)
}

/// The kv value of a pending durable report: the agent message, encoded.
pub fn encode_pending_report(host: &str, report_id: u64, msg: &agent_message::Msg) -> String {
    use prost::Message;
    let bytes = pb::AgentMessage {
        msg: Some(msg.clone()),
    }
    .encode_to_vec();
    serde_json::json!({"host": host, "report_id": report_id, "message": hex::encode(bytes)})
        .to_string()
}

fn decode_pending_report(value: &str) -> Option<(String, u64, agent_message::Msg)> {
    use prost::Message;
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    let host = v.get("host")?.as_str()?.to_owned();
    let report_id = v.get("report_id")?.as_u64()?;
    let bytes = hex::decode(v.get("message")?.as_str()?).ok()?;
    let msg = pb::AgentMessage::decode(bytes.as_slice()).ok()?.msg?;
    Some((host, report_id, msg))
}

/// The HMAC approval token the Apply message carries.
pub fn approval_token(key: &[u8; 32], rollout: &str, phase: &str, digest: &str) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else {
        return String::new();
    };
    mac.update(format!("{rollout}|{phase}|{digest}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Loads live rollouts from the store and starts the controller.
pub async fn spawn(
    deps: ControllerDeps,
    cancel: CancellationToken,
) -> buckle_core::Result<(ControllerHandle, tokio::task::JoinHandle<()>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut controller = Controller::load(deps).await?;
    let handle = ControllerHandle { tx };
    let task = tokio::spawn(async move {
        // Durable reports the gRPC layer stored but whose reaction was not
        // saved before the server stopped (they were never acked: the ack
        // waits for that checkpoint). Replay them before any other input.
        for (host, report_id, msg) in std::mem::take(&mut controller.replay) {
            tracing::info!(buckle.host = %host, report = report_id, "replaying a durable report stored before a restart");
            controller
                .handle(Input::Durable {
                    host,
                    report_id,
                    msg,
                    done: None,
                })
                .await;
        }
        controller.drive().await;
        controller.flush().await;
        controller.run(rx, cancel).await;
    });
    Ok((handle, task))
}

struct Live {
    record: RolloutRecord,
    hosts: Vec<RolloutHostRecord>,
    approvals: Vec<ApprovalRecord>,
    revision: Arc<Revision>,
    span: Span,
    phase_spans: HashMap<usize, Span>,
    host_spans: HashMap<String, Span>,
    dirty: bool,
    dirty_hosts: BTreeSet<usize>,
}

impl Live {
    fn find(&self, host: &str) -> Option<usize> {
        self.hosts.iter().position(|h| h.host == host)
    }

    fn phase_hosts(&self, phase: usize) -> Vec<usize> {
        (0..self.hosts.len())
            .filter(|&i| self.hosts[i].runtime.phase_index == Some(phase))
            .collect()
    }

    fn stragglers_index(&self) -> Option<usize> {
        self.record
            .runtime
            .phases
            .iter()
            .position(|p| p.virtual_stage)
    }
}

enum Out {
    Msg(String, ServerMessage),
    AssignAndPlan {
        host: String,
        revision: String,
        stable: bool,
    },
}

struct Controller {
    store: Store,
    link: Arc<dyn HostLink>,
    clock: Arc<dyn Clock>,
    events: EventBus,
    revisions: Arc<RevisionCache>,
    secrets: ServerSecrets,
    key: [u8; 32],
    options: ControllerOptions,
    live: Vec<Live>,
    outbox: Vec<Out>,
    audits: Vec<NewAudit>,
    stable: Option<String>,
    stable_dirty: bool,
    excluded: HashSet<String>,
    /// Each host's status and effective labels as last seen, so a host
    /// update that changes neither (e.g. notes) is not acted on.
    host_seen: HashMap<String, (HostStatus, BTreeMap<String, String>)>,
    /// The last checkpoint failed: retry it at this time.
    flush_retry: Option<DateTime<Utc>>,
    /// Consecutive failed checkpoints (for the retry backoff).
    flush_failures: u32,
    /// Durable reports already reacted to by this process, by (host, id).
    consumed: HashSet<(String, u64)>,
    /// Pending-report keys to delete with the next checkpoint.
    pending_clear: Vec<String>,
    /// Waiting for the next successful checkpoint to ack their reports.
    ack_waiters: Vec<oneshot::Sender<bool>>,
    /// Pending durable reports found on load, replayed before any input.
    replay: Vec<(String, u64, agent_message::Msg)>,
}

fn msg(m: server_message::Msg) -> ServerMessage {
    ServerMessage { msg: Some(m) }
}

fn timestamp(t: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: t.timestamp(),
        nanos: t.timestamp_subsec_nanos() as i32,
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn check_name(r: &pb::HealthResult) -> String {
    format!("{}/{}", r.module, r.check)
}

/// A required, failing (not unknown), non-waived check.
fn is_failure(r: &pb::HealthResult, waived: &[String]) -> bool {
    r.required && !r.passed && !r.unknown && !waived.contains(&check_name(r))
}

/// A host a rollback applies the previous stable revision to: one this
/// rollout sent an Apply, unless an operator excluded (skipped) it.
fn rollback_target(h: &RolloutHostRecord) -> bool {
    h.runtime.applied && h.state != HostPhase::Skipped
}

/// Host states that wait on the agent: the disconnect clock starts for them
/// on load (after a restart) and when their session drops, so a host still
/// to plan or apply is not taken for offline, and missed, inside the
/// reconnect grace.
fn awaits_reconnect(state: HostPhase) -> bool {
    state.in_flight()
        || matches!(
            state,
            HostPhase::RollingBack | HostPhase::PlanPending | HostPhase::Planned
        )
}

fn traceparent_of(span: &Span) -> Option<String> {
    use tracing_batteries::prelude::{OpenTelemetrySpanExt, get_text_map_propagator};
    let cx = span.context();
    let mut map: HashMap<String, String> = HashMap::new();
    get_text_map_propagator(|p| p.inject_context(&cx, &mut map));
    map.remove("traceparent").filter(|s| !s.is_empty())
}

fn trace_id_of(span: &Span) -> Option<String> {
    use tracing_batteries::prelude::{OpenTelemetrySpanExt, TraceContextExt};
    let cx = span.context();
    let sc = cx.span().span_context().clone();
    sc.is_valid().then(|| sc.trace_id().to_string())
}

fn run_span(id: &str, revision: &str, traceparent: Option<&str>) -> Span {
    use tracing_batteries::prelude::{OpenTelemetrySpanExt, get_text_map_propagator};
    let span = tracing::info_span!(parent: None, "rollout.run", buckle.rollout = %id, buckle.revision = %revision);
    if let Some(tp) = traceparent {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("traceparent".into(), tp.to_owned());
        let parent = get_text_map_propagator(|p| p.extract(&map));
        let _ = span.set_parent(parent);
    }
    span
}

impl Controller {
    async fn load(deps: ControllerDeps) -> buckle_core::Result<Self> {
        let (records, stable, excluded) = deps
            .store
            .call(|db| {
                let live = db.rollouts().live()?;
                let mut out = Vec::new();
                for r in live {
                    let hosts = db.rollouts().hosts(&r.id)?;
                    let approvals = db.approvals().for_rollout(&r.id)?;
                    out.push((r, hosts, approvals));
                }
                let all = db.hosts().list()?;
                let excluded: HashSet<String> = all
                    .iter()
                    .filter(|h| h.status != HostStatus::Active)
                    .map(|h| h.id.clone())
                    .collect();
                let seen: HashMap<String, (HostStatus, BTreeMap<String, String>)> = all
                    .iter()
                    .map(|h| (h.id.clone(), (h.status, h.effective_labels())))
                    .collect();
                let pending = db.kv().with_prefix(keys::PENDING_REPORT_PREFIX)?;
                Ok((
                    out,
                    db.kv().get(keys::STABLE_REVISION)?,
                    (excluded, seen, pending),
                ))
            })
            .await?;
        let (excluded, host_seen, pending) = excluded;
        let mut replay = Vec::new();
        for (key, value) in pending {
            match decode_pending_report(&value) {
                Some(r) => replay.push(r),
                None => {
                    tracing::warn!(key = %key, "a pending durable report could not be decoded; ignoring it")
                }
            }
        }
        // Oldest first (report ids are monotonic per agent).
        replay.sort_by_key(|(_, id, _)| *id);
        let now = deps.clock.now();
        let mut live = Vec::new();
        let mut unloadable: Vec<(RolloutRecord, NewAudit)> = Vec::new();
        for (mut record, mut hosts, approvals) in records {
            let revision = match deps.revisions.load(&record.revision).await {
                Ok(r) => r,
                Err(e) => {
                    // It can never make progress: end it, saying why, rather
                    // than leaving it looking live for ever.
                    tracing::error!(rollout = %record.id, error = %e, "a live rollout's revision could not be loaded; aborting it");
                    let why = format!(
                        "revision {} could not be loaded at start-up: {}",
                        short(&record.revision),
                        e.description()
                    );
                    let from = crate::store::rollouts::state_str(record.state);
                    record.state = RolloutState::Aborted;
                    record.finished_at = Some(now);
                    record.runtime.stragglers_open = false;
                    record.runtime.end_reason = Some(why.clone());
                    let audit = NewAudit {
                        at: now,
                        actor: "controller".into(),
                        action: "rollout.aborted".into(),
                        outcome: "ok".into(),
                        subject: Some(record.id.clone()),
                        message: format!("rollout {from} → aborted: {why}"),
                        detail: Some(serde_json::json!({"from": from, "to": "aborted"})),
                        trace_id: record.runtime.trace_id.clone(),
                    };
                    unloadable.push((record, audit));
                    continue;
                }
            };
            // Every session is gone after a restart: start the disconnect clock
            // for hosts that are waiting on an agent, and for hosts still to
            // plan or apply, so they are not taken for offline (and missed)
            // before their agents had a chance to reconnect.
            let mut dirty_hosts = BTreeSet::new();
            let gate_pending: HashSet<String> = record
                .runtime
                .phases
                .iter()
                .filter_map(|p| p.gate.as_ref())
                .flat_map(|g| g.pending.keys().cloned())
                .collect();
            for (i, h) in hosts.iter_mut().enumerate() {
                if (awaits_reconnect(h.state) || gate_pending.contains(&h.host))
                    && h.runtime.disconnected_at.is_none()
                {
                    h.runtime.disconnected_at = Some(now);
                    dirty_hosts.insert(i);
                }
            }
            let span = run_span(
                &record.id,
                &record.revision,
                record.runtime.traceparent.as_deref(),
            );
            live.push(Live {
                record,
                hosts,
                approvals,
                revision,
                span,
                phase_spans: HashMap::new(),
                host_spans: HashMap::new(),
                dirty: false,
                dirty_hosts,
            });
        }
        if !unloadable.is_empty() {
            deps.store
                .transaction(move |db| {
                    for (r, a) in &unloadable {
                        db.rollouts().save(r)?;
                        db.audit().add(a)?;
                    }
                    Ok(())
                })
                .await?;
        }
        Ok(Self {
            store: deps.store,
            link: deps.link,
            clock: deps.clock,
            events: deps.events,
            revisions: deps.revisions,
            secrets: deps.secrets,
            key: deps.approval_key,
            options: deps.options,
            live,
            outbox: Vec::new(),
            audits: Vec::new(),
            stable,
            stable_dirty: false,
            excluded,
            host_seen,
            flush_retry: None,
            flush_failures: 0,
            consumed: HashSet::new(),
            pending_clear: Vec::new(),
            ack_waiters: Vec::new(),
            replay,
        })
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<Input>, cancel: CancellationToken) {
        loop {
            let deadline = self.next_deadline();
            let input = tokio::select! {
                _ = cancel.cancelled() => break,
                i = rx.recv() => match i {
                    Some(i) => Some(i),
                    None => break,
                },
                _ = async {
                    match deadline {
                        Some(d) => self.clock.sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => None,
            };
            if let Some(input) = input {
                self.handle(input).await;
            }
            self.drive().await;
            self.flush().await;
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn connected(&self, host: &str) -> bool {
        self.link.is_connected(host)
    }

    fn eligible(&self, host: &str) -> bool {
        self.connected(host) && !self.excluded.contains(host)
    }

    /// Whether a host still to plan or apply counts as missing its phase:
    /// excluded, or disconnected and not inside a reconnect grace (after a
    /// restart every host is disconnected until its agent comes back; the
    /// disconnect clock started by [`Controller::load`] bounds the wait).
    fn offline_for_phase(&self, h: &RolloutHostRecord) -> bool {
        self.excluded.contains(&h.host)
            || (!self.connected(&h.host) && h.runtime.disconnected_at.is_none())
    }

    /// Whether host `hi` may be sent an Apply (or a baseline check leading
    /// to one) now: the rollout is running (not paused, rolling back or
    /// finished), or the host is in the open stragglers stage, and the host
    /// is connected and not excluded.
    fn may_apply(&self, ri: usize, hi: usize) -> bool {
        let record = &self.live[ri].record;
        let open = if self.is_virtual(ri, hi) {
            record.runtime.stragglers_open
        } else {
            record.is_running() || self.released_while_paused(ri, hi)
        };
        open && self.eligible(&self.live[ri].hosts[hi].host)
    }

    /// An operator released host `hi` (retry, continue) while the rollout is
    /// paused by a failure: it proceeds although nothing else starts.
    fn released_while_paused(&self, ri: usize, hi: usize) -> bool {
        let record = &self.live[ri].record;
        record.state == RolloutState::Paused
            && self.live[ri].hosts[hi].runtime.released
            && record
                .runtime
                .pause
                .as_ref()
                .is_none_or(|p| p.kind != "operator")
    }

    /// Returns a host whose baseline check or apply cannot go ahead (the
    /// rollout stopped running) to planned, so it starts again from its
    /// baseline when the rollout resumes.
    fn defer_host(&mut self, ri: usize, hi: usize, why: &str) {
        if let Some(pi) = self.live[ri].hosts[hi].runtime.phase_index
            && let Some(p) = self.live[ri].record.runtime.phases.get_mut(pi)
        {
            p.started = p.started.saturating_sub(1);
            self.live[ri].dirty = true;
        }
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.request_id = None;
            h.runtime.applied = false;
            h.runtime.apply_started_at = None;
        }
        self.set_host_state(ri, hi, HostPhase::Planned, why);
    }

    // ------------------------------------------------------------ bookkeeping

    fn audit(
        &mut self,
        actor: &str,
        action: &str,
        rollout: &str,
        message: String,
        detail: serde_json::Value,
        trace_id: Option<String>,
    ) {
        self.audits.push(NewAudit {
            at: self.now(),
            actor: actor.to_owned(),
            action: action.to_owned(),
            outcome: "ok".into(),
            subject: Some(rollout.to_owned()),
            message,
            detail: Some(detail),
            trace_id,
        });
    }

    fn touch_host(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        let live = &mut self.live[ri];
        live.hosts[hi].updated_at = now;
        live.dirty_hosts.insert(hi);
    }

    fn set_host_state(&mut self, ri: usize, hi: usize, state: HostPhase, why: &str) {
        let (from, host, phase, rollout, trace) = {
            let live = &self.live[ri];
            let h = &live.hosts[hi];
            if h.state == state {
                return;
            }
            (
                h.state,
                h.host.clone(),
                h.phase.clone(),
                live.record.id.clone(),
                live.record.runtime.trace_id.clone(),
            )
        };
        self.live[ri].hosts[hi].state = state;
        self.touch_host(ri, hi);
        if state.finished() || state == HostPhase::Held {
            self.live[ri].host_spans.remove(&host);
            self.live[ri].hosts[hi].runtime.released = false;
        }
        let message = if why.is_empty() {
            format!("{host}: {} → {}", from.as_str(), state.as_str())
        } else {
            format!("{host}: {} → {} ({why})", from.as_str(), state.as_str())
        };
        tracing::info!(buckle.rollout = %rollout, buckle.host = %host, buckle.phase = %phase, from = from.as_str(), to = state.as_str(), "rollout host transition");
        // Ordinary progress (planned, applying, baking, ...) reaches the UI
        // as a live host event from the checkpoint; only outcomes (healthy,
        // failed, held, skipped, rolled back, missed) go to the audit log.
        let action = format!("rollout.host.{}", state.as_str());
        if buckle_api::is_progress_action("controller", &action) {
            return;
        }
        self.audit(
            "controller",
            &action,
            &rollout,
            message,
            serde_json::json!({"host": host, "phase": phase, "from": from.as_str(), "to": state.as_str()}),
            trace,
        );
    }

    fn set_rollout_state(&mut self, ri: usize, state: RolloutState, actor: &str, why: &str) {
        let live = &mut self.live[ri];
        if live.record.state == state {
            return;
        }
        // The display moving between planning, rolling and baking as waves
        // come and go is progress: live events only, not the audit log.
        let progress = |s: RolloutState| {
            matches!(
                s,
                RolloutState::Planning | RolloutState::Rolling | RolloutState::Baking
            )
        };
        if why.is_empty() && progress(live.record.state) && progress(state) {
            tracing::debug!(buckle.rollout = %live.record.id, from = ?live.record.state, to = ?state, "rollout progress");
            live.record.state = state;
            live.dirty = true;
            return;
        }
        let from = live.record.state;
        live.record.state = state;
        live.dirty = true;
        let id = live.record.id.clone();
        let trace = live.record.runtime.trace_id.clone();
        let from_s = crate::store::rollouts::state_str(from);
        let to_s = crate::store::rollouts::state_str(state);
        tracing::info!(buckle.rollout = %id, from = %from_s, to = %to_s, "rollout transition");
        let message = if why.is_empty() {
            format!("rollout {from_s} → {to_s}")
        } else {
            format!("rollout {from_s} → {to_s}: {why}")
        };
        self.audit(
            actor,
            &format!("rollout.{to_s}"),
            &id,
            message,
            serde_json::json!({"from": from_s, "to": to_s}),
            trace,
        );
    }

    fn host_span(&mut self, ri: usize, hi: usize) -> Span {
        let live = &mut self.live[ri];
        let host = live.hosts[hi].host.clone();
        if let Some(s) = live.host_spans.get(&host) {
            return s.clone();
        }
        let pi = live.hosts[hi].runtime.phase_index.unwrap_or(usize::MAX);
        let phase_name = live.hosts[hi].phase.clone();
        let parent = live
            .phase_spans
            .entry(pi)
            .or_insert_with(|| {
                tracing::info_span!(parent: &live.span, "rollout.phase", buckle.phase = %phase_name)
            })
            .clone();
        let wave = live.hosts[hi].wave.unwrap_or(0);
        let span = tracing::info_span!(parent: &parent, "rollout.host", buckle.host = %host, buckle.phase = %phase_name, buckle.wave = wave);
        live.host_spans.insert(host, span.clone());
        span
    }

    fn send(&mut self, host: &str, m: server_message::Msg) {
        self.outbox.push(Out::Msg(host.to_owned(), msg(m)));
    }

    // ------------------------------------------------------------ inputs

    async fn handle(&mut self, input: Input) {
        match input {
            Input::Connected(c) => self.on_connected(c).await,
            Input::Disconnected(host) => self.on_disconnected(&host),
            Input::Agent(host, m) => match m {
                agent_message::Msg::Plan(p) => self.on_plan(&host, p),
                agent_message::Msg::Result(r) => self.on_apply_result(&host, r),
                agent_message::Msg::Health(h) => self.on_health(&host, h),
                agent_message::Msg::Rollback(r) => self.on_local_rollback(&host, r),
                _ => {}
            },
            Input::RevisionAdded(id) => self.on_revision_added(&id).await,
            Input::HostChanged(host) => self.on_host_changed(&host).await,
            Input::HostRegistered(host) => self.on_host_registered(&host).await,
            Input::Durable {
                host,
                report_id,
                msg,
                done,
            } => {
                let key = (host.clone(), report_id);
                if self.consumed.insert(key) {
                    match msg {
                        agent_message::Msg::Result(r) => self.on_apply_result(&host, r),
                        agent_message::Msg::Rollback(r) => self.on_local_rollback(&host, r),
                        _ => {}
                    }
                }
                // Removed with the checkpoint that holds the reaction (again,
                // harmlessly, for a report already consumed).
                self.pending_clear
                    .push(pending_report_key(&host, report_id));
                if let Some(done) = done {
                    self.ack_waiters.push(done);
                }
            }
            Input::Command {
                actor,
                command,
                reply,
            } => {
                let action = command.action();
                let result = self.on_command(&actor, command).await;
                if let Err(e) = &result {
                    self.audits.push(NewAudit {
                        at: self.now(),
                        actor: actor.clone(),
                        action: action.to_owned(),
                        outcome: "refused".into(),
                        subject: None,
                        message: e.error.description().to_string(),
                        detail: None,
                        trace_id: None,
                    });
                }
                // Flush before replying so the caller observes the new state.
                self.drive().await;
                let saved = self.flush().await;
                let result = match result {
                    Ok(_) if !saved => Err(CommandError::internal(not_saved())),
                    other => other,
                };
                let _ = reply.send(result);
            }
        }
    }

    async fn on_connected(&mut self, c: Connected) {
        let now = self.now();
        for ri in 0..self.live.len() {
            let Some(hi) = self.live[ri].find(&c.host) else {
                continue;
            };
            self.live[ri].hosts[hi].runtime.disconnected_at = None;
            self.touch_host(ri, hi);
            if self.excluded.contains(&c.host) {
                continue;
            }
            let state = self.live[ri].hosts[hi].state;
            let target = self.live[ri].hosts[hi].runtime.revision.clone();
            let active =
                self.live[ri].record.is_active() || self.live[ri].record.runtime.stragglers_open;
            if !active {
                continue;
            }
            match state {
                HostPhase::PlanPending => self.request_plan(ri, hi),
                HostPhase::Baseline if self.may_apply(ri, hi) => self.send_baseline(ri, hi),
                HostPhase::Baseline => {
                    self.defer_host(ri, hi, "reconnected while the rollout is not running")
                }
                HostPhase::Applying => {
                    // The agent skips an Apply whose session ended before it
                    // started, so the server re-sends it (same plan digest,
                    // same approval token) unless the host already shows the
                    // target revision (it finished: bake) or announces the
                    // apply as running or its result as still to come.
                    if c.applied_revision.as_deref() == Some(target.as_str()) {
                        self.start_bake(ri, hi);
                    } else if !c.new_session || c.unreported_apply_revisions.contains(&target) {
                        tracing::debug!(host = %c.host, "the apply is running or its result is on its way");
                    } else if let Some(run) = self.finished_run(ri, hi).await {
                        // The result was stored but never reached the
                        // controller (e.g. a server restart in between).
                        let why = run
                            .error
                            .unwrap_or_else(|| format!("apply {}", run.outcome));
                        self.fail_host(ri, hi, &why);
                    } else if !self.may_apply(ri, hi) {
                        // Never ran, and the rollout no longer starts work.
                        self.defer_host(
                            ri,
                            hi,
                            "the apply never ran and the rollout is not running",
                        );
                    } else if self.live[ri].hosts[hi].runtime.apply_resends >= MAX_APPLY_RESENDS {
                        let why = format!(
                            "the agent reconnected {} times without running the approved apply",
                            MAX_APPLY_RESENDS
                        );
                        self.fail_host(ri, hi, &why);
                    } else {
                        self.resend_apply(ri, hi);
                    }
                }
                HostPhase::Baking if self.bake_seconds(ri, hi) > 0 => {
                    // Resume the bake. Past its window, a host that was away
                    // reports afresh before it counts as baked, and a check
                    // still failing is followed through settle.
                    let until = self.live[ri].hosts[hi].runtime.bake_until.unwrap_or(now);
                    if until > now {
                        self.send_bake_request(ri, hi, until);
                    } else if c.new_session {
                        let h = &mut self.live[ri].hosts[hi];
                        h.runtime.bake_reports = 0;
                        h.runtime.bake_until = Some(now);
                        self.send_bake_request(ri, hi, now);
                    }
                }
                HostPhase::RollingBack => {
                    let prev = self.live[ri].record.runtime.previous_stable.clone();
                    if prev.is_some() && c.applied_revision == prev {
                        self.set_host_state(
                            ri,
                            hi,
                            HostPhase::RolledBack,
                            "reconnected on the previous stable revision",
                        );
                    } else {
                        self.send_rollback(ri, hi);
                    }
                }
                _ => {}
            }
            // A pending pre-phase gate check.
            let pending = self.live[ri].record.runtime.phases.iter().find_map(|p| {
                p.gate
                    .as_ref()
                    .and_then(|g| g.pending.get(&c.host).cloned())
            });
            if let Some(request_id) = pending {
                let tp = traceparent_of(&self.live[ri].span).unwrap_or_default();
                self.send(
                    &c.host,
                    server_message::Msg::Health(RunHealth {
                        request_id,
                        scope: HealthScope::Rollout as i32,
                        modules: vec![],
                        until: None,
                        traceparent: tp,
                    }),
                );
            }
        }
        self.maybe_add_straggler(&c.host, c.applied_revision.as_deref())
            .await;
    }

    fn on_disconnected(&mut self, host: &str) {
        let now = self.now();
        for ri in 0..self.live.len() {
            let gate_pending = self.live[ri].record.runtime.phases.iter().any(|p| {
                p.gate
                    .as_ref()
                    .is_some_and(|g| g.pending.contains_key(host))
            });
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let state = self.live[ri].hosts[hi].state;
            if awaits_reconnect(state) || gate_pending {
                self.live[ri].hosts[hi].runtime.disconnected_at = Some(now);
                self.touch_host(ri, hi);
            }
        }
    }

    fn on_plan(&mut self, host: &str, plan: PlanResult) {
        for ri in 0..self.live.len() {
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let h = &self.live[ri].hosts[hi];
            // The agent could not plan the rollback (it reports a failed
            // plan for the previous stable revision): the rollback failed.
            if h.state == HostPhase::RollingBack
                && !plan.error.is_empty()
                && self.live[ri].record.runtime.previous_stable.as_deref()
                    == Some(plan.revision.as_str())
            {
                self.live[ri].hosts[hi].runtime.error =
                    Some(format!("rollback failed: planning failed: {}", plan.error));
                self.set_host_state(
                    ri,
                    hi,
                    HostPhase::Failed,
                    "rollback failed; needs attention",
                );
                continue;
            }
            if h.runtime.revision != plan.revision {
                continue;
            }
            // The agent refused an Apply because its fresh plan differs from
            // the approved digest: the fresh plan comes back as a PlanResult
            // whose error starts with "stale plan". The host returns to
            // planned with the new digest (re-approval where required).
            let stale = plan.error.starts_with("stale plan") && !plan.digest.is_empty();
            if stale && matches!(h.state, HostPhase::Applying | HostPhase::Baseline) {
                self.live[ri].hosts[hi].runtime.applied = false;
                self.live[ri].hosts[hi].state = HostPhase::PlanPending;
            } else if !matches!(h.state, HostPhase::PlanPending | HostPhase::Planned) {
                continue;
            }
            if !plan.error.is_empty() && !stale {
                let error = format!("planning failed: {}", plan.error);
                self.live[ri].hosts[hi].runtime.error = Some(error.clone());
                if self.later_phase(ri, hi) {
                    // Shown now, charged to its phase's budget once that
                    // phase starts (drive_phases), so an earlier phase is
                    // not paused for it.
                    self.set_host_state(ri, hi, HostPhase::Failed, &error);
                } else {
                    self.fail_host(ri, hi, &error);
                }
                continue;
            }
            let plan_json: Option<serde_json::Value> = serde_json::from_slice(&plan.plan_json).ok();
            {
                let h = &mut self.live[ri].hosts[hi];
                h.plan_digest = Some(plan.digest.clone());
                h.plan = plan_json;
                h.runtime.plan_changes = Some(plan.changes);
                h.runtime.plan_removals = Some(plan.removals);
                h.runtime.destructive = plan.destructive;
                h.runtime.deferred_modules = plan.deferred_modules.clone();
                // A new plan: what failed before no longer describes it.
                h.runtime.failed_checks.clear();
                h.runtime.failed_module = None;
                // A valid plan with modules that could not be planned: shown
                // on the host; applying it fails those modules only.
                h.runtime.error = (!plan.module_errors.is_empty()).then(|| {
                    format!(
                        "modules that could not be planned: {}",
                        plan.module_errors.join("; ")
                    )
                });
            }
            self.touch_host(ri, hi);
            self.set_host_state(ri, hi, HostPhase::Planned, "plan received");
        }
    }

    fn on_apply_result(&mut self, host: &str, result: ApplyResult) {
        for ri in 0..self.live.len() {
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let state = self.live[ri].hosts[hi].state;
            let target = self.live[ri].hosts[hi].runtime.revision.clone();
            let prev = self.live[ri].record.runtime.previous_stable.clone();
            match state {
                HostPhase::Applying
                    if result.revision == target
                        && (result.plan_digest.is_empty()
                            || self.live[ri].hosts[hi].plan_digest.as_deref()
                                == Some(result.plan_digest.as_str())) =>
                {
                    self.live[ri].hosts[hi].result =
                        serde_json::to_value(ResultView::from(&result)).ok();
                    if result.succeeded {
                        self.start_bake(ri, hi);
                    } else {
                        {
                            let h = &mut self.live[ri].hosts[hi];
                            h.runtime.failed_module = (!result.failed_module.is_empty())
                                .then(|| result.failed_module.clone());
                            h.runtime.failed_checks = result.failed_checks.clone();
                            let detail = if result.error.is_empty() {
                                "apply failed".to_owned()
                            } else {
                                result.error.clone()
                            };
                            // Blocked steps: failed for the rollout, but
                            // nothing was rolled back; an operator must look.
                            h.runtime.needs_attention = result.needs_attention;
                            h.runtime.error = Some(if result.needs_attention {
                                format!("needs attention: steps stayed blocked: {detail}")
                            } else {
                                detail
                            });
                        }
                        let why = self.live[ri].hosts[hi]
                            .runtime
                            .error
                            .clone()
                            .unwrap_or_default();
                        self.fail_host(ri, hi, &why);
                    }
                }
                // Only the result of applying the previous stable revision is
                // the rollback's: a forward result that failed and left the
                // host on that revision (applied_revision) is not.
                HostPhase::RollingBack if prev.as_deref() == Some(result.revision.as_str()) => {
                    if result.succeeded {
                        self.set_host_state(
                            ri,
                            hi,
                            HostPhase::RolledBack,
                            "previous stable revision applied",
                        );
                    } else {
                        self.live[ri].hosts[hi].runtime.error =
                            Some(format!("rollback failed: {}", result.error));
                        self.set_host_state(
                            ri,
                            hi,
                            HostPhase::Failed,
                            "rollback failed; needs attention",
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn on_health(&mut self, host: &str, report: HealthReport) {
        let scope = HealthScope::try_from(report.scope).unwrap_or(HealthScope::Unspecified);
        let now = self.now();
        tracing::debug!(buckle.host = %host, scope = ?scope, request = %report.request_id, revision = %report.revision, results = report.results.len(), "health report");
        for ri in 0..self.live.len() {
            // Pre-phase gate answers.
            let gate_phase = self.live[ri].record.runtime.phases.iter().position(|p| {
                p.gate
                    .as_ref()
                    .and_then(|g| g.pending.get(host))
                    .is_some_and(|id| {
                        scope == HealthScope::Rollout
                            && (report.request_id.is_empty() || *id == report.request_id)
                    })
            });
            if let Some(pi) = gate_phase {
                let waived = self.live[ri]
                    .find(host)
                    .map(|hi| self.live[ri].hosts[hi].runtime.waived_checks.clone())
                    .unwrap_or_default();
                let failures: Vec<String> = report
                    .results
                    .iter()
                    .filter(|r| is_failure(r, &waived))
                    .map(check_name)
                    .collect();
                let phase_name = self.live[ri].record.runtime.phases[pi].name.clone();
                if failures.is_empty() {
                    if let Some(g) = self.live[ri].record.runtime.phases[pi].gate.as_mut() {
                        g.pending.remove(host);
                        g.passed.push(host.to_owned());
                    }
                    self.live[ri].dirty = true;
                } else {
                    let why = format!(
                        "pre-phase gate after '{phase_name}': failing {}",
                        failures.join(", ")
                    );
                    if let Some(hi) = self.live[ri].find(host) {
                        self.live[ri].hosts[hi].health =
                            serde_json::to_value(HealthView::from(&report)).ok();
                        self.live[ri].hosts[hi].runtime.failed_checks = failures.clone();
                    }
                    self.fail_gate(ri, pi, host, &why);
                }
                continue;
            }
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let state = self.live[ri].hosts[hi].state;
            let waived = self.live[ri].hosts[hi].runtime.waived_checks.clone();
            match (scope, state) {
                (HealthScope::Baseline, HostPhase::Baseline)
                    if self.live[ri].hosts[hi].runtime.request_id.as_deref()
                        == Some(report.request_id.as_str())
                        || report.request_id.is_empty() =>
                {
                    self.live[ri].hosts[hi].health =
                        serde_json::to_value(HealthView::from(&report)).ok();
                    let failures: Vec<String> = report
                        .results
                        .iter()
                        .filter(|r| is_failure(r, &waived))
                        .map(check_name)
                        .collect();
                    if failures.is_empty() {
                        self.send_apply(ri, hi);
                    } else {
                        self.live[ri].hosts[hi].runtime.failed_checks = failures.clone();
                        let why = format!("baseline failing: {}", failures.join(", "));
                        match self.live[ri].record.policy.health.baseline {
                            BaselinePolicy::Hold => {
                                self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                                self.live[ri].hosts[hi].runtime.held_at_baseline = true;
                                self.set_host_state(ri, hi, HostPhase::Held, &why);
                            }
                            BaselinePolicy::Skip => {
                                self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                                self.set_host_state(ri, hi, HostPhase::Skipped, &why);
                            }
                            BaselinePolicy::Ignore => self.send_apply(ri, hi),
                        }
                    }
                }
                // A bake report belongs to this rollout's bake of the host:
                // its revision and request (when the agent names them).
                (HealthScope::Bake, HostPhase::Baking)
                    if (report.revision.is_empty()
                        || report.revision == self.live[ri].hosts[hi].runtime.revision)
                        && (report.request_id.is_empty()
                            || self.live[ri].hosts[hi]
                                .runtime
                                .bake_requests
                                .contains(&report.request_id)) =>
                {
                    self.live[ri].hosts[hi].health =
                        serde_json::to_value(HealthView::from(&report)).ok();
                    let h = &mut self.live[ri].hosts[hi];
                    h.runtime.bake_reports = h.runtime.bake_reports.saturating_add(1);
                    for r in &report.results {
                        let name = check_name(r);
                        if is_failure(r, &waived) {
                            h.runtime.failing_since.entry(name).or_insert(now);
                        } else if r.passed && h.runtime.failing_since.remove(&name).is_some() {
                            h.runtime.flaps.push(name);
                        }
                    }
                    self.touch_host(ri, hi);
                }
                _ => {}
            }
        }
    }

    fn on_local_rollback(&mut self, host: &str, lr: LocalRollback) {
        for ri in 0..self.live.len() {
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let h = &self.live[ri].hosts[hi];
            if !(h.runtime.applied || h.state.in_flight())
                || h.runtime.revision != lr.from_revision && !lr.from_revision.is_empty()
            {
                continue;
            }
            let what = if lr.modules.is_empty() {
                format!(
                    "rolled itself back from {} to {}",
                    short(&lr.from_revision),
                    short(&lr.to_revision)
                )
            } else {
                format!(
                    "rolled module(s) {} back to {}",
                    lr.modules.join(", "),
                    short(&lr.to_revision)
                )
            };
            // Design §10: a rollback that itself fails leaves the host in
            // needs attention with both results recorded.
            let why = if lr.succeeded {
                format!("{what}: {}", lr.reason)
            } else {
                format!(
                    "needs attention: {what}: {}; the rollback itself failed: {}",
                    lr.reason,
                    if lr.rollback_error.is_empty() {
                        "no detail"
                    } else {
                        lr.rollback_error.as_str()
                    }
                )
            };
            {
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.error = Some(why.clone());
                h.runtime.needs_attention = !lr.succeeded;
                if !lr.modules.is_empty() {
                    h.runtime.failed_module = lr.modules.first().cloned();
                }
                h.runtime.failed_checks = lr
                    .evidence
                    .iter()
                    .filter(|r| !r.passed)
                    .map(check_name)
                    .collect();
                h.result = serde_json::to_value(RollbackView::from(&lr)).ok();
            }
            self.set_host_state(ri, hi, HostPhase::Failed, &why);
            let is_virtual = self.is_virtual(ri, hi);
            if !is_virtual {
                self.pause(
                    ri,
                    "local_rollback",
                    format!("{host} {why}"),
                    Some(host.to_owned()),
                );
            }
        }
    }

    async fn on_revision_added(&mut self, id: &str) {
        let id_owned = id.to_owned();
        let record = match self
            .store
            .call(move |db| db.revisions().get(&id_owned))
            .await
        {
            Ok(Some(r)) => r,
            _ => return,
        };
        if record.status != RevisionStatus::Valid {
            return;
        }
        let revision = match self.revisions.load(id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(revision = %id, error = %e, "a new revision could not be loaded");
                return;
            }
        };
        if revision.fleet.rollout.trigger == buckle_core::model::RolloutTrigger::Auto
            && let Err(e) = self.start(id, "controller", "").await
        {
            tracing::warn!(revision = %id, error = %e, "automatic rollout did not start");
        }
    }

    async fn on_host_registered(&mut self, host: &str) {
        let h = host.to_owned();
        let Some(record) = self
            .store
            .call(move |db| db.hosts().get(&h))
            .await
            .ok()
            .flatten()
        else {
            return;
        };
        if record.status == HostStatus::Active {
            self.excluded.remove(host);
        } else {
            self.excluded.insert(host.to_owned());
        }
        self.host_seen
            .insert(host.to_owned(), (record.status, record.effective_labels()));
    }

    async fn on_host_changed(&mut self, host: &str) {
        let h = host.to_owned();
        let record = self
            .store
            .call(move |db| db.hosts().get(&h))
            .await
            .ok()
            .flatten();
        match &record {
            Some(r) if r.status == HostStatus::Active => {
                self.excluded.remove(host);
            }
            _ => {
                self.excluded.insert(host.to_owned());
            }
        }
        if record.is_none() {
            self.host_seen.remove(host);
            self.forget_host(host);
            return;
        }
        // Only a change the agent and the rollout care about (status or
        // effective labels; not notes) is pushed and re-sends in-flight work.
        let seen = record
            .as_ref()
            .map(|r| (r.status, r.effective_labels()))
            .expect("checked above");
        if self.host_seen.get(host) == Some(&seen) {
            return;
        }
        self.host_seen.insert(host.to_owned(), seen);
        if let Some(r) = &record
            && self.connected(host)
        {
            self.push_host_update(r).await;
        }
        if let Some(r) = record
            && r.status == HostStatus::Active
            && self.connected(host)
        {
            self.on_connected(Connected {
                host: host.to_owned(),
                applied_revision: r.applied_revision.clone(),
                last_plan_digest: r.last_plan_digest.clone(),
                ..Default::default()
            })
            .await;
        }
    }

    /// A forgotten host leaves every live rollout it has not finished:
    /// skipped (it no longer exists, so it is neither waited for nor counted
    /// as a failure) and dropped from pending gates.
    fn forget_host(&mut self, host: &str) {
        for ri in 0..self.live.len() {
            let mut gated = false;
            for p in &mut self.live[ri].record.runtime.phases {
                if let Some(g) = p.gate.as_mut() {
                    gated |= g.pending.remove(host).is_some();
                }
            }
            self.live[ri].dirty |= gated;
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let state = self.live[ri].hosts[hi].state;
            if matches!(
                state,
                HostPhase::Healthy | HostPhase::Skipped | HostPhase::RolledBack
            ) {
                continue;
            }
            {
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.error = Some("the host was forgotten".into());
                h.runtime.disconnected_at = None;
                h.runtime.request_id = None;
            }
            self.set_host_state(ri, hi, HostPhase::Skipped, "the host was forgotten");
            self.unpause_if_cleared(ri, "controller");
        }
    }

    /// Tells a connected agent what changed about its host record (status,
    /// effective labels and, when active, the stable revision's secrets), as
    /// Welcome would on the next session (design §08 "Registration").
    async fn push_host_update(&self, record: &crate::store::HostRecord) {
        let mut update = pb::HostUpdate {
            status: crate::store::hosts_status_str(record.status).to_owned(),
            labels: record.effective_labels().into_iter().collect(),
            ..Default::default()
        };
        if let Some(stable) = self.stable.clone() {
            update.stable_revision = stable.clone();
            let id = stable.clone();
            update.stable_tree_digest = self
                .store
                .call(move |db| Ok(db.revisions().get(&id)?.and_then(|r| r.tarball_digest)))
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            if record.status == HostStatus::Active
                && let Ok(rev) = self.revisions.load(&stable).await
            {
                let h = record.id.clone();
                let facts = self
                    .store
                    .call(move |db| Ok(db.hosts().latest_facts(&h)?.map(|(f, _)| f)))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(serde_json::Value::Null);
                let ctx = crate::fleet::record_context(record, &facts, &rev.fleet);
                update.secrets = Some(self.secrets.for_host(&rev, &ctx).await);
            }
        }
        self.link
            .send(&record.id, msg(server_message::Msg::HostUpdate(update)));
    }

    // ------------------------------------------------------------ commands

    fn find_rollout(&self, id: &str) -> Result<usize, CommandError> {
        self.live
            .iter()
            .position(|l| l.record.id == id)
            .ok_or_else(|| CommandError::not_found(format!("No active rollout '{id}'.")))
    }

    fn find_host(&self, ri: usize, host: &str) -> Result<usize, CommandError> {
        self.live[ri].find(host).ok_or_else(|| {
            CommandError::not_found(format!("Host '{host}' is not part of this rollout."))
        })
    }

    fn command_audit(
        &mut self,
        actor: &str,
        action: &str,
        ri: usize,
        message: String,
        detail: serde_json::Value,
    ) {
        let id = self.live[ri].record.id.clone();
        let trace = self.live[ri].record.runtime.trace_id.clone();
        self.audit(actor, action, &id, message, detail, trace);
    }

    async fn on_command(&mut self, actor: &str, command: Command) -> Result<String, CommandError> {
        match command {
            Command::Start { revision, note } => self.start(&revision, actor, &note).await,
            Command::Approve {
                rollout,
                phase,
                digests,
                note,
            } => self.approve(actor, &rollout, &phase, digests, &note).await,
            Command::Pause { rollout, note } => {
                let ri = self.find_rollout(&rollout)?;
                if !self.live[ri].record.is_running() {
                    return Err(CommandError::conflict(
                        "Only a running rollout can be paused.",
                        &["Check the rollout's state; it may already be paused or finished."],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.pause",
                    ri,
                    format!("{actor} paused the rollout. {note}"),
                    serde_json::json!({"note": note}),
                );
                self.pause(ri, "operator", format!("paused by {actor}"), None);
                Ok(rollout)
            }
            Command::Resume { rollout, note } => {
                let ri = self.find_rollout(&rollout)?;
                if self.live[ri].record.state != RolloutState::Paused {
                    return Err(CommandError::conflict(
                        "Only a paused rollout can be resumed.",
                        &["Check the rollout's state."],
                    ));
                }
                // Accept the failures that caused the pause, in the current
                // phase and in earlier ones (a pre-phase gate can fail a host
                // of an earlier phase).
                let current = self.live[ri].record.runtime.current_phase;
                for p in 0..self.live[ri].record.runtime.phases.len() {
                    if p > current || self.live[ri].record.runtime.phases[p].virtual_stage {
                        continue;
                    }
                    let (failures, budget) = self.phase_failures(ri, p);
                    if failures > budget {
                        self.live[ri].record.runtime.phases[p].accepted_failures +=
                            failures - budget;
                        self.live[ri].dirty = true;
                    }
                }
                self.command_audit(
                    actor,
                    "rollout.resume",
                    ri,
                    format!("{actor} resumed the rollout. {note}"),
                    serde_json::json!({"note": note}),
                );
                self.unpause(ri, actor);
                Ok(rollout)
            }
            Command::Rollback { rollout, note } => {
                let ri = self.find_rollout(&rollout)?;
                if !self.live[ri].record.is_active()
                    || self.live[ri].record.state == RolloutState::RollingBack
                {
                    return Err(CommandError::conflict(
                        "This rollout cannot be rolled back in its current state.",
                        &["Only a running or paused rollout can be rolled back."],
                    ));
                }
                if self.live[ri].record.runtime.previous_stable.is_none() {
                    return Err(CommandError::conflict(
                        "There is no previous stable revision to roll back to.",
                        &[
                            "This was the fleet's first rollout; abort it instead, or start a rollout of an older revision.",
                        ],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.rollback",
                    ri,
                    format!("{actor} started a rollback. {note}"),
                    serde_json::json!({"note": note}),
                );
                self.begin_rollback(ri, actor);
                Ok(rollout)
            }
            Command::Reject { rollout, reason } => {
                let ri = self.find_rollout(&rollout)?;
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(CommandError::invalid(
                        "Say why the rollout is rejected.",
                        &["The reason is recorded in the audit log."],
                    ));
                }
                if self.live[ri].record.state != RolloutState::AwaitingApproval {
                    return Err(CommandError::conflict(
                        format!(
                            "Rollout '{rollout}' is not awaiting approval, so there is nothing to reject."
                        ),
                        &["Abort it instead."],
                    ));
                }
                let phase = self.live[ri]
                    .record
                    .runtime
                    .phases
                    .get(self.live[ri].record.runtime.current_phase)
                    .map(|p| p.name.clone());
                self.command_audit(
                    actor,
                    "rollout.reject",
                    ri,
                    format!("{actor} rejected the rollout: {reason}"),
                    serde_json::json!({"reason": reason, "phase": phase}),
                );
                self.finish(
                    ri,
                    RolloutState::Aborted,
                    actor,
                    &format!("rejected by {actor}: {reason}"),
                );
                Ok(rollout)
            }
            Command::Abort { rollout, note } => {
                let ri = self.find_rollout(&rollout)?;
                if self.live[ri].record.is_terminal() {
                    return Err(CommandError::conflict(
                        "This rollout has already finished.",
                        &[],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.abort",
                    ri,
                    format!("{actor} aborted the rollout. {note}"),
                    serde_json::json!({"note": note}),
                );
                self.finish(
                    ri,
                    RolloutState::Aborted,
                    actor,
                    &format!("aborted by {actor}"),
                );
                Ok(rollout)
            }
            Command::Retry {
                rollout,
                host,
                note,
            } => {
                let ri = self.find_rollout(&rollout)?;
                let hi = self.find_host(ri, &host)?;
                if !matches!(
                    self.live[ri].hosts[hi].state,
                    HostPhase::Failed | HostPhase::Held | HostPhase::Skipped | HostPhase::Missed
                ) {
                    return Err(CommandError::conflict(
                        format!(
                            "Host '{host}' is not failed, held or skipped, so there is nothing to retry."
                        ),
                        &[],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.host.retry",
                    ri,
                    format!("{actor} retried {host}. {note}"),
                    serde_json::json!({"host": host, "note": note}),
                );
                self.replan(ri, hi, actor);
                Ok(rollout)
            }
            Command::Continue {
                rollout,
                host,
                waive,
                note,
            } => {
                let ri = self.find_rollout(&rollout)?;
                let hi = self.find_host(ri, &host)?;
                let h = &self.live[ri].hosts[hi];
                if h.state != HostPhase::Failed || h.runtime.failed_checks.is_empty() {
                    return Err(CommandError::conflict(
                        format!("Host '{host}' has no failed health checks to waive."),
                        &[
                            "Continue is offered only for health failures; use retry or skip for task errors.",
                        ],
                    ));
                }
                if waive.is_empty() {
                    return Err(CommandError::invalid(
                        "Name at least one check (module/check) to waive.",
                        &["Pass the failed checks shown on the host."],
                    ));
                }
                let unknown: Vec<&String> = waive
                    .iter()
                    .filter(|w| !h.runtime.failed_checks.contains(w))
                    .collect();
                if !unknown.is_empty() {
                    return Err(CommandError::invalid(
                        format!(
                            "Host '{host}' did not fail {}; only its failed checks ({}) can be waived.",
                            unknown
                                .iter()
                                .map(|w| w.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                            h.runtime.failed_checks.join(", ")
                        ),
                        &["Pass the failed checks shown on the host."],
                    ));
                }
                {
                    let h = &mut self.live[ri].hosts[hi];
                    for w in &waive {
                        if !h.runtime.waived_checks.contains(w) {
                            h.runtime.waived_checks.push(w.clone());
                        }
                    }
                }
                self.command_audit(
                    actor,
                    "rollout.host.continue",
                    ri,
                    format!(
                        "{actor} continued {host} waiving {}. {note}",
                        waive.join(", ")
                    ),
                    serde_json::json!({"host": host, "waived": waive, "note": note}),
                );
                self.replan(ri, hi, actor);
                Ok(rollout)
            }
            Command::Skip {
                rollout,
                host,
                note,
            } => {
                let ri = self.find_rollout(&rollout)?;
                let hi = self.find_host(ri, &host)?;
                let state = self.live[ri].hosts[hi].state;
                if matches!(
                    state,
                    HostPhase::Healthy | HostPhase::Skipped | HostPhase::RolledBack
                ) {
                    return Err(CommandError::conflict(
                        format!("Host '{host}' has already finished."),
                        &[],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.host.skip",
                    ri,
                    format!("{actor} skipped {host}. {note}"),
                    serde_json::json!({"host": host, "note": note}),
                );
                self.live[ri].hosts[hi].runtime.error = Some(if note.is_empty() {
                    format!("skipped by {actor}")
                } else {
                    format!("skipped by {actor}: {note}")
                });
                self.set_host_state(ri, hi, HostPhase::Skipped, "skipped by an operator");
                self.unpause_if_cleared(ri, actor);
                Ok(rollout)
            }
            Command::Force {
                rollout,
                host,
                note,
            } => {
                let ri = self.find_rollout(&rollout)?;
                let hi = self.find_host(ri, &host)?;
                if self.live[ri].hosts[hi].state != HostPhase::Held {
                    return Err(CommandError::conflict(
                        format!("Host '{host}' is not held at baseline."),
                        &[],
                    ));
                }
                if !self.connected(&host) {
                    return Err(CommandError::conflict(
                        format!("Host '{host}' is not connected."),
                        &["Wait for the agent to reconnect, then force it again."],
                    ));
                }
                if !self.may_apply(ri, hi) {
                    return Err(CommandError::conflict(
                        format!("Rollout '{rollout}' is not running, so nothing is applied."),
                        &["Resume the rollout first, then force the host."],
                    ));
                }
                self.command_audit(
                    actor,
                    "rollout.host.force",
                    ri,
                    format!("{actor} forced {host} past its failing baseline. {note}"),
                    serde_json::json!({"host": host, "note": note}),
                );
                self.live[ri].hosts[hi].runtime.force = true;
                self.send_apply(ri, hi);
                Ok(rollout)
            }
        }
    }

    async fn approve(
        &mut self,
        actor: &str,
        rollout: &str,
        phase: &str,
        digests: Vec<String>,
        note: &str,
    ) -> Result<String, CommandError> {
        let ri = self.find_rollout(rollout)?;
        if self.live[ri].record.is_terminal() && !self.live[ri].record.runtime.stragglers_open {
            return Err(CommandError::conflict(
                "This rollout has already finished.",
                &[],
            ));
        }
        let Some(pi) = self.live[ri]
            .record
            .runtime
            .phases
            .iter()
            .position(|p| p.name == phase)
        else {
            return Err(CommandError::not_found(format!(
                "This rollout has no phase '{phase}'."
            )));
        };
        let hosts = self.live[ri].phase_hosts(pi);
        let planned: Vec<(String, String)> = hosts
            .iter()
            .filter_map(|&i| {
                let h = &self.live[ri].hosts[i];
                (!h.state.finished()).then(|| h.plan_digest.clone().map(|d| (h.host.clone(), d)))?
            })
            .collect();
        if planned.is_empty() {
            return Err(CommandError::conflict(
                format!("No host in phase '{phase}' has a plan to approve yet."),
                &["Wait for the plans to arrive; offline hosts show as plan pending."],
            ));
        }
        let uncovered: Vec<String> = planned
            .iter()
            .filter(|(_, d)| !digests.contains(d))
            .map(|(h, d)| format!("{h} ({})", short(d)))
            .collect();
        if !uncovered.is_empty() {
            return Err(CommandError::conflict(
                format!(
                    "The approval does not cover every plan in phase '{phase}': {}.",
                    uncovered.join(", ")
                ),
                &[
                    "Reload the rollout and review the current plans; a host's plan may have changed.",
                ],
            ));
        }
        let now = self.now();
        let id = self.live[ri].record.id.clone();
        let (p2, a2, d2, n2) = (
            phase.to_owned(),
            actor.to_owned(),
            digests.clone(),
            note.to_owned(),
        );
        let id2 = id.clone();
        let approval_id = self
            .store
            .call(move |db| db.approvals().add(&id2, &p2, &a2, &d2, &n2, now))
            .await
            .map_err(CommandError::internal)?;
        self.live[ri].approvals.push(ApprovalRecord {
            id: approval_id,
            rollout: id.clone(),
            phase: phase.to_owned(),
            approver: actor.to_owned(),
            plan_digests: digests.clone(),
            note: note.to_owned(),
            at: now,
        });
        self.live[ri].dirty = true;
        for &i in &hosts {
            self.live[ri].dirty_hosts.insert(i);
        }
        self.command_audit(
            actor,
            "rollout.approve",
            ri,
            format!(
                "{actor} approved phase '{phase}' ({}). {note}",
                digests
                    .iter()
                    .map(|d| short(d))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            serde_json::json!({"phase": phase, "plan_digests": digests, "note": note}),
        );
        Ok(id)
    }

    // ------------------------------------------------------------ rollout lifecycle

    async fn start(
        &mut self,
        revision: &str,
        actor: &str,
        note: &str,
    ) -> Result<String, CommandError> {
        let rid = revision.to_owned();
        let record = self
            .store
            .call(move |db| db.revisions().get(&rid))
            .await
            .map_err(CommandError::internal)?
            .ok_or_else(|| CommandError::not_found(format!("No revision '{revision}'.")))?;
        if record.status != RevisionStatus::Valid {
            return Err(CommandError::conflict(
                format!(
                    "Revision {} was rejected and cannot be rolled out.",
                    short(revision)
                ),
                &["Fix the validation errors shown on the revision and push a new commit."],
            ));
        }
        let rev = self
            .revisions
            .load(revision)
            .await
            .map_err(CommandError::internal)?;
        let policy = rev.fleet.rollout.clone();
        let now = self.now();
        let id = new_id();
        let busy = self
            .live
            .iter()
            .any(|l| l.record.is_active() || l.record.state == RolloutState::Detected);
        let record = RolloutRecord {
            id: id.clone(),
            revision: revision.to_owned(),
            state: RolloutState::Detected,
            policy: policy.clone(),
            started_at: now,
            finished_at: None,
            triggered_by: actor.to_owned(),
            runtime: RolloutRuntime {
                note: note.to_owned(),
                revision_message: record.message.lines().next().unwrap_or_default().to_owned(),
                ..Default::default()
            },
        };
        let span = run_span(&id, revision, None);
        self.live.push(Live {
            record,
            hosts: Vec::new(),
            approvals: Vec::new(),
            revision: rev,
            span,
            phase_spans: HashMap::new(),
            host_spans: HashMap::new(),
            dirty: true,
            dirty_hosts: BTreeSet::new(),
        });
        let ri = self.live.len() - 1;
        {
            let trace = trace_id_of(&self.live[ri].span);
            let tp = traceparent_of(&self.live[ri].span);
            self.live[ri].record.runtime.trace_id = trace;
            self.live[ri].record.runtime.traceparent = tp;
        }
        self.command_audit(
            actor,
            "rollout.detected",
            ri,
            format!("rollout of {} created by {actor}", short(revision)),
            serde_json::json!({"revision": revision}),
        );
        if busy {
            match policy.supersede {
                SupersedePolicy::Queue => {
                    for rj in 0..ri {
                        if self.live[rj].record.state == RolloutState::Detected {
                            self.finish(
                                rj,
                                RolloutState::Aborted,
                                "controller",
                                &format!("superseded by {id}"),
                            );
                        }
                    }
                    return Ok(id);
                }
                SupersedePolicy::Replace => {
                    for rj in 0..ri {
                        if self.live[rj].record.is_active()
                            || self.live[rj].record.state == RolloutState::Detected
                        {
                            self.finish(
                                rj,
                                RolloutState::Aborted,
                                "controller",
                                &format!("superseded by {id}"),
                            );
                        }
                    }
                }
            }
        }
        self.freeze(ri).await.map_err(CommandError::internal)?;
        Ok(id)
    }

    /// Freezes the target set and requests plans everywhere.
    async fn freeze(&mut self, ri: usize) -> buckle_core::Result<()> {
        let hosts = self
            .store
            .call(|db| {
                let mut out = Vec::new();
                for h in db.hosts().list()? {
                    let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                    out.push((h, facts.unwrap_or(serde_json::Value::Null)));
                }
                Ok(out)
            })
            .await?;
        let now = self.now();
        let live = &mut self.live[ri];
        let fleet = &live.revision.fleet;
        let policy = &live.record.policy;
        let phases = crate::fleet::effective_phases(policy);
        live.record.runtime.phases = phases
            .iter()
            .map(|p| PhaseRuntime {
                name: p.name.clone(),
                state: PhaseState::Pending,
                parallel: p.parallel,
                approval: p.approval,
                bake_seconds: p.bake.unwrap_or(policy.health.bake).as_secs(),
                max_failures: p.max_failures,
                virtual_stage: false,
                accepted_failures: 0,
                started: 0,
                gate: None,
                bake_until: None,
            })
            .collect();
        live.record.runtime.previous_stable = self.stable.clone();
        live.record.started_at = now;
        let mut untargeted = Vec::new();
        for (host, facts) in hosts {
            self.host_seen
                .insert(host.id.clone(), (host.status, host.effective_labels()));
            if host.status != HostStatus::Active {
                continue;
            }
            let ctx = crate::fleet::record_context(&host, &facts, fleet);
            match crate::fleet::phase_for(&ctx, &phases) {
                Some(pi) => live.hosts.push(RolloutHostRecord {
                    rollout: live.record.id.clone(),
                    host: host.id.clone(),
                    phase: phases[pi].name.clone(),
                    wave: None,
                    state: HostPhase::PlanPending,
                    plan_digest: None,
                    plan: None,
                    result: None,
                    health: None,
                    created_at: now,
                    updated_at: now,
                    runtime: HostRuntime {
                        revision: live.record.revision.clone(),
                        phase_index: Some(pi),
                        ..Default::default()
                    },
                }),
                None => untargeted.push(host.id.clone()),
            }
        }
        live.record.runtime.untargeted = untargeted;
        live.dirty = true;
        live.dirty_hosts = (0..live.hosts.len()).collect();
        let n = live.hosts.len();
        let count = format!("{n} hosts frozen into {} phases", phases.len());
        self.set_rollout_state(ri, RolloutState::Planning, "controller", &count);
        for hi in 0..n {
            self.request_plan(ri, hi);
        }
        Ok(())
    }

    fn request_plan(&mut self, ri: usize, hi: usize) {
        let host = self.live[ri].hosts[hi].host.clone();
        if !self.eligible(&host) {
            return;
        }
        let revision = self.live[ri].hosts[hi].runtime.revision.clone();
        let stable = self.stable.as_deref() == Some(revision.as_str());
        self.outbox.push(Out::AssignAndPlan {
            host,
            revision,
            stable,
        });
    }

    fn replan(&mut self, ri: usize, hi: usize, actor: &str) {
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.failed_checks.clear();
            h.runtime.failed_module = None;
            h.runtime.error = None;
            h.runtime.needs_attention = false;
            h.runtime.failing_since.clear();
            h.runtime.request_id = None;
            h.runtime.bake_until = None;
            h.runtime.attempts += 1;
            h.runtime.released = true;
            h.plan_digest = None;
        }
        self.set_host_state(ri, hi, HostPhase::PlanPending, "re-planning");
        self.request_plan(ri, hi);
        self.unpause_if_cleared(ri, actor);
    }

    fn pause(&mut self, ri: usize, kind: &str, reason: String, host: Option<String>) {
        if !self.live[ri].record.is_running() {
            return;
        }
        let now = self.now();
        self.live[ri].record.runtime.pause = Some(PauseInfo {
            kind: kind.to_owned(),
            reason: reason.clone(),
            host,
            at: now,
        });
        self.set_rollout_state(ri, RolloutState::Paused, "controller", &reason);
    }

    fn unpause(&mut self, ri: usize, actor: &str) {
        if self.live[ri].record.state != RolloutState::Paused {
            return;
        }
        self.live[ri].record.runtime.pause = None;
        self.set_rollout_state(ri, RolloutState::Rolling, actor, "resumed");
    }

    /// Operator decisions on a host resume a paused rollout (design: "Paused
    /// → Rolling: resume or retry host") once what paused it is cleared: never
    /// an operator's own pause, never while a failure budget is still breached,
    /// and for a gate or local-rollback pause only once the host it named is
    /// no longer failed.
    fn unpause_if_cleared(&mut self, ri: usize, actor: &str) {
        if self.live[ri].record.state == RolloutState::Paused && self.pause_cleared(ri) {
            self.unpause(ri, actor);
        }
    }

    fn pause_cleared(&self, ri: usize) -> bool {
        let live = &self.live[ri];
        let Some(pause) = &live.record.runtime.pause else {
            return true;
        };
        if pause.kind == "operator" || self.breach(ri).is_some() {
            return false;
        }
        match pause.kind.as_str() {
            "failure_budget" => true,
            "gate" | "local_rollback" => pause.host.as_ref().is_none_or(|h| {
                live.find(h).is_none_or(|i| {
                    !matches!(live.hosts[i].state, HostPhase::Failed | HostPhase::Held)
                })
            }),
            _ => false,
        }
    }

    /// Failed hosts in phase `p` and the failures it tolerates.
    fn phase_failures(&self, ri: usize, p: usize) -> (u32, u32) {
        let hosts = self.live[ri].phase_hosts(p);
        let failures = hosts
            .iter()
            .filter(|&&i| self.live[ri].hosts[i].state == HostPhase::Failed)
            .count() as u32;
        let budget = self.live[ri].record.runtime.phases[p].failure_budget(hosts.len());
        (failures, budget)
    }

    /// The first phase up to the current one whose failures exceed its
    /// budget, as (phase, failures, budget), unless `on_failure: continue`.
    fn breach(&self, ri: usize) -> Option<(usize, u32, u32)> {
        let record = &self.live[ri].record;
        if record.policy.on_failure == FailurePolicy::Continue {
            return None;
        }
        let current = record.runtime.current_phase;
        (0..record.runtime.phases.len())
            .filter(|&p| p <= current && !record.runtime.phases[p].virtual_stage)
            .find_map(|p| {
                let (failures, budget) = self.phase_failures(ri, p);
                (failures > budget).then_some((p, failures, budget))
            })
    }

    /// Acts on a standing failure-budget breach per `on_failure`.
    fn on_breach(&mut self, ri: usize, p: usize, failures: u32, latest: Option<(String, String)>) {
        let phase = &self.live[ri].record.runtime.phases[p];
        let reason = match &latest {
            Some((host, why)) => format!(
                "phase '{}': {failures} failed host(s) exceed max_failures {} (latest: {host}: {why})",
                phase.name,
                view::parallel_str(phase.max_failures)
            ),
            None => format!(
                "phase '{}': {failures} failed host(s) exceed max_failures {}",
                phase.name,
                view::parallel_str(phase.max_failures)
            ),
        };
        let host = latest.map(|(h, _)| h);
        match self.live[ri].record.policy.on_failure {
            FailurePolicy::Pause => self.pause(ri, "failure_budget", reason, host),
            FailurePolicy::Rollback if self.live[ri].record.runtime.previous_stable.is_some() => {
                self.begin_rollback(ri, "controller")
            }
            FailurePolicy::Rollback => self.pause(ri, "failure_budget", reason, host),
            FailurePolicy::Continue => {}
        }
    }

    fn finish(&mut self, ri: usize, state: RolloutState, actor: &str, why: &str) {
        let now = self.now();
        self.live[ri].record.finished_at = Some(now);
        self.live[ri].record.runtime.end_reason = Some(why.to_owned());
        self.live[ri].record.runtime.stragglers_open = false;
        self.live[ri].dirty = true;
        self.live[ri].host_spans.clear();
        self.live[ri].phase_spans.clear();
        self.set_rollout_state(ri, state, actor, why);
    }

    fn begin_rollback(&mut self, ri: usize, actor: &str) {
        let last = self.live[ri]
            .hosts
            .iter()
            .filter(|h| rollback_target(h))
            .filter_map(|h| h.runtime.phase_index)
            .max();
        self.live[ri].record.runtime.rollback_phase = last;
        self.live[ri].record.runtime.pause = None;
        self.set_rollout_state(
            ri,
            RolloutState::RollingBack,
            actor,
            "applying the previous stable revision to updated hosts, newest phase first",
        );
    }

    // ------------------------------------------------------------ per host steps

    fn approved(&self, ri: usize, hi: usize) -> bool {
        let live = &self.live[ri];
        let h = &live.hosts[hi];
        let phase = h
            .runtime
            .phase_index
            .and_then(|i| live.record.runtime.phases.get(i));
        !view::needs_approval(phase, h, &live.approvals)
    }

    fn start_baseline(&mut self, ri: usize, hi: usize) {
        let pi = self.live[ri].hosts[hi].runtime.phase_index.unwrap_or(0);
        let parallel = {
            let n = self.live[ri].phase_hosts(pi).len();
            self.live[ri].record.runtime.phases[pi].parallel.resolve(n) as u32
        };
        let started = self.live[ri].record.runtime.phases[pi].started;
        self.live[ri].record.runtime.phases[pi].started += 1;
        self.live[ri].dirty = true;
        self.live[ri].hosts[hi].wave = Some(started / parallel.max(1) + 1);
        self.set_host_state(ri, hi, HostPhase::Baseline, "baseline health requested");
        self.send_baseline(ri, hi);
    }

    fn send_baseline(&mut self, ri: usize, hi: usize) {
        let request_id = new_id();
        self.live[ri].hosts[hi].runtime.request_id = Some(request_id.clone());
        self.touch_host(ri, hi);
        let span = self.host_span(ri, hi);
        let host = self.live[ri].hosts[hi].host.clone();
        self.send(
            &host,
            server_message::Msg::Health(RunHealth {
                request_id,
                scope: HealthScope::Baseline as i32,
                modules: vec![],
                until: None,
                traceparent: traceparent_of(&span).unwrap_or_default(),
            }),
        );
    }

    /// The Apply for host `hi`: its approved plan digest and the approval
    /// token over (rollout, phase, digest), identical every time it is built.
    fn build_apply(&mut self, ri: usize, hi: usize) -> Apply {
        let span = self.host_span(ri, hi);
        let live = &self.live[ri];
        let h = &live.hosts[hi];
        let digest = h.plan_digest.clone().unwrap_or_default();
        let phase = h.phase.clone();
        let rollout = live.record.id.clone();
        Apply {
            revision: h.runtime.revision.clone(),
            plan_digest: digest.clone(),
            approval_token: approval_token(&self.key, &rollout, &phase, &digest),
            waived_checks: h.runtime.waived_checks.clone(),
            rollout_id: rollout,
            phase,
            force: h.runtime.force,
            traceparent: traceparent_of(&span).unwrap_or_default(),
            select: PLAN_SELECT.to_owned(),
        }
    }

    /// A finished, stored run of host `hi`'s approved plan since its Apply was
    /// sent, when one exists (the agent acknowledged its result).
    async fn finished_run(&self, ri: usize, hi: usize) -> Option<crate::store::RunRecord> {
        let h = &self.live[ri].hosts[hi];
        let (host, digest, revision) = (
            h.host.clone(),
            h.plan_digest.clone()?,
            h.runtime.revision.clone(),
        );
        let since = h.runtime.apply_started_at?;
        let runs = self
            .store
            .call(move |db| db.runs().for_host(&host, 20))
            .await
            .ok()?;
        runs.into_iter()
            .find(|r| r.plan_digest == digest && r.revision == revision && r.finished_at >= since)
    }

    /// Re-sends the Apply of a host that reconnected without having run it.
    fn resend_apply(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        let apply = self.build_apply(ri, hi);
        let host = self.live[ri].hosts[hi].host.clone();
        self.live[ri].hosts[hi].runtime.apply_started_at = Some(now);
        self.live[ri].hosts[hi].runtime.apply_resends += 1;
        self.touch_host(ri, hi);
        tracing::info!(host = %host, digest = %short(&apply.plan_digest), "re-sending the apply after a reconnect");
        self.send(&host, server_message::Msg::Apply(apply));
    }

    fn send_apply(&mut self, ri: usize, hi: usize) {
        if !self.may_apply(ri, hi) {
            self.defer_host(
                ri,
                hi,
                "the rollout is not running; the apply waits for it to resume",
            );
            return;
        }
        let now = self.now();
        let apply = self.build_apply(ri, hi);
        let h = &mut self.live[ri].hosts[hi];
        h.runtime.applied = true;
        h.runtime.apply_started_at = Some(now);
        h.runtime.apply_resends = 0;
        let host = h.host.clone();
        self.set_host_state(ri, hi, HostPhase::Applying, "apply sent");
        self.touch_host(ri, hi);
        self.send(&host, server_message::Msg::Apply(apply));
    }

    fn start_bake(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        let pi = self.live[ri].hosts[hi].runtime.phase_index.unwrap_or(0);
        let bake = self.live[ri]
            .record
            .runtime
            .phases
            .get(pi)
            .map(|p| p.bake_seconds)
            .unwrap_or(0);
        let until = now + chrono::Duration::seconds(bake as i64);
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.bake_until = Some(until);
            h.runtime.failing_since.clear();
            h.runtime.bake_reports = 0;
            h.runtime.bake_requests.clear();
        }
        self.set_host_state(ri, hi, HostPhase::Baking, "applied; baking");
        if bake > 0 {
            self.send_bake_request(ri, hi, until);
        }
    }

    /// The bake window of host `hi`'s phase, in seconds.
    fn bake_seconds(&self, ri: usize, hi: usize) -> u64 {
        self.live[ri].hosts[hi]
            .runtime
            .phase_index
            .and_then(|i| self.live[ri].record.runtime.phases.get(i))
            .map(|p| p.bake_seconds)
            .unwrap_or(0)
    }

    /// How long after its bake window a connected host that never sent a
    /// bake report is given before it fails.
    fn bake_report_grace(&self, ri: usize) -> chrono::Duration {
        let settle =
            chrono::Duration::from_std(self.live[ri].record.policy.settle).unwrap_or_default();
        settle.max(chrono::Duration::minutes(1))
    }

    fn send_bake_request(&mut self, ri: usize, hi: usize, until: DateTime<Utc>) {
        let span = self.host_span(ri, hi);
        let host = self.live[ri].hosts[hi].host.clone();
        let request_id = new_id();
        {
            let requests = &mut self.live[ri].hosts[hi].runtime.bake_requests;
            requests.push(request_id.clone());
            // A handful is plenty (one per reconnect).
            if requests.len() > 8 {
                requests.remove(0);
            }
        }
        self.touch_host(ri, hi);
        self.send(
            &host,
            server_message::Msg::Health(RunHealth {
                request_id,
                scope: HealthScope::Bake as i32,
                modules: vec![],
                until: Some(timestamp(until)),
                traceparent: traceparent_of(&span).unwrap_or_default(),
            }),
        );
    }

    fn send_rollback(&mut self, ri: usize, hi: usize) {
        let Some(prev) = self.live[ri].record.runtime.previous_stable.clone() else {
            return;
        };
        let span = self.host_span(ri, hi);
        let host = self.live[ri].hosts[hi].host.clone();
        let rollout = self.live[ri].record.id.clone();
        self.send(
            &host,
            server_message::Msg::Rollback(pb::Rollback {
                revision: prev,
                reason: format!("rollout {rollout} is being rolled back"),
                rollout_id: rollout,
                traceparent: traceparent_of(&span).unwrap_or_default(),
            }),
        );
    }

    /// Host `hi` belongs to a phase after the one being worked on.
    fn later_phase(&self, ri: usize, hi: usize) -> bool {
        !self.is_virtual(ri, hi)
            && self.live[ri].hosts[hi]
                .runtime
                .phase_index
                .is_some_and(|p| p > self.live[ri].record.runtime.current_phase)
    }

    fn is_virtual(&self, ri: usize, hi: usize) -> bool {
        self.live[ri].hosts[hi]
            .runtime
            .phase_index
            .and_then(|i| self.live[ri].record.runtime.phases.get(i))
            .is_some_and(|p| p.virtual_stage)
    }

    fn fail_host(&mut self, ri: usize, hi: usize, why: &str) {
        if self.live[ri].hosts[hi].runtime.error.is_none() {
            self.live[ri].hosts[hi].runtime.error = Some(why.to_owned());
        }
        self.set_host_state(ri, hi, HostPhase::Failed, why);
        if self.is_virtual(ri, hi) {
            return;
        }
        let Some(pi) = self.live[ri].hosts[hi].runtime.phase_index else {
            return;
        };
        let (failures, budget) = self.phase_failures(ri, pi);
        if failures <= budget {
            return;
        }
        let host = self.live[ri].hosts[hi].host.clone();
        self.on_breach(ri, pi, failures, Some((host, why.to_owned())));
    }

    // ------------------------------------------------------------ timers

    fn next_deadline(&self) -> Option<DateTime<Utc>> {
        let now = self.now();
        let settle =
            |l: &Live| chrono::Duration::from_std(l.record.policy.settle).unwrap_or_default();
        let timeout =
            chrono::Duration::from_std(self.options.disconnect_timeout).unwrap_or_default();
        let apply_timeout =
            chrono::Duration::from_std(self.options.apply_timeout).unwrap_or_default();
        let mut next: Option<DateTime<Utc>> = None;
        let mut consider = |t: DateTime<Utc>| {
            next = Some(next.map_or(t, |n| n.min(t)));
        };
        if let Some(t) = self.flush_retry {
            consider(t);
        }
        for l in &self.live {
            for h in &l.hosts {
                if h.state == HostPhase::Baking {
                    for since in h.runtime.failing_since.values() {
                        consider(*since + settle(l));
                    }
                    // A window already past is a deadline only when it can
                    // complete the host now; while a check is followed through
                    // settle it would spin (settle is the deadline then), and
                    // a host that has not reported is waited for a grace.
                    let window = h
                        .runtime
                        .phase_index
                        .and_then(|i| l.record.runtime.phases.get(i))
                        .is_some_and(|p| p.bake_seconds > 0);
                    if let Some(u) = h.runtime.bake_until {
                        if u > now
                            || (h.runtime.failing_since.is_empty()
                                && (!window
                                    || (h.runtime.bake_reports > 0 && self.connected(&h.host))))
                        {
                            consider(u);
                        } else if window && h.runtime.bake_reports == 0 {
                            let grace = settle(l).max(chrono::Duration::minutes(1));
                            if u + grace > now {
                                consider(u + grace);
                            }
                        }
                    }
                }
                if let Some(d) = h.runtime.disconnected_at {
                    consider(d + timeout);
                }
                if h.state == HostPhase::Applying
                    && let Some(started) = h.runtime.apply_started_at
                {
                    consider(started + apply_timeout);
                }
            }
        }
        next
    }

    fn on_time(&mut self) {
        let now = self.now();
        let timeout =
            chrono::Duration::from_std(self.options.disconnect_timeout).unwrap_or_default();
        let apply_timeout =
            chrono::Duration::from_std(self.options.apply_timeout).unwrap_or_default();
        for ri in 0..self.live.len() {
            if self.live[ri].record.is_terminal() && !self.live[ri].record.runtime.stragglers_open {
                continue;
            }
            let settle =
                chrono::Duration::from_std(self.live[ri].record.policy.settle).unwrap_or_default();
            for hi in 0..self.live[ri].hosts.len() {
                let h = &self.live[ri].hosts[hi];
                let host = h.host.clone();
                if h.state == HostPhase::Baking {
                    let regressed: Vec<String> = h
                        .runtime
                        .failing_since
                        .iter()
                        .filter(|(_, since)| **since + settle <= now)
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !regressed.is_empty() {
                        self.live[ri].hosts[hi].runtime.failed_checks = regressed.clone();
                        let why = format!(
                            "bake regression: {} failing for longer than settle",
                            regressed.join(", ")
                        );
                        self.fail_host(ri, hi, &why);
                        continue;
                    }
                    // Baked: the window passed without a required check
                    // failing, and the host is connected and has reported
                    // (when it has a window at all).
                    let window = self.bake_seconds(ri, hi) > 0;
                    let reported = !window || h.runtime.bake_reports > 0;
                    let ended = h.runtime.bake_until.filter(|u| *u <= now);
                    if let Some(until) = ended
                        && h.runtime.failing_since.is_empty()
                    {
                        if reported && (!window || self.connected(&host)) {
                            self.set_host_state(ri, hi, HostPhase::Healthy, "baked");
                            continue;
                        }
                        if !reported
                            && self.connected(&host)
                            && until + self.bake_report_grace(ri) <= now
                        {
                            let why = "sent no health report during its bake".to_owned();
                            self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                            self.fail_host(ri, hi, &why);
                            continue;
                        }
                    }
                }
                let h = &self.live[ri].hosts[hi];
                if h.state == HostPhase::Applying
                    && h.runtime
                        .apply_started_at
                        .is_some_and(|started| started + apply_timeout <= now)
                {
                    let mins = self.options.apply_timeout.as_secs() / 60;
                    let why = format!("no apply result within {mins}m");
                    self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                    self.fail_host(ri, hi, &why);
                    continue;
                }
                let h = &self.live[ri].hosts[hi];
                if h.runtime
                    .disconnected_at
                    .is_some_and(|d| d + timeout <= now)
                    && self.connected(&host)
                {
                    // Back (its reconnect was handled elsewhere): not a
                    // deadline any more.
                    self.live[ri].hosts[hi].runtime.disconnected_at = None;
                    self.touch_host(ri, hi);
                    continue;
                }
                let h = &self.live[ri].hosts[hi];
                if let Some(d) = h.runtime.disconnected_at
                    && d + timeout <= now
                    && !self.connected(&host)
                {
                    let state = h.state;
                    self.live[ri].hosts[hi].runtime.disconnected_at = None;
                    self.touch_host(ri, hi);
                    let mins = self.options.disconnect_timeout.as_secs() / 60;
                    let why = format!("disconnected for more than {mins}m");
                    if state.in_flight() {
                        self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                        self.fail_host(ri, hi, &why);
                    } else if state == HostPhase::RollingBack {
                        self.live[ri].hosts[hi].runtime.error =
                            Some(format!("rollback failed: {why}"));
                        self.set_host_state(
                            ri,
                            hi,
                            HostPhase::Failed,
                            "rollback failed; needs attention",
                        );
                    }
                    let gate = self.live[ri].record.runtime.phases.iter().position(|p| {
                        p.gate
                            .as_ref()
                            .is_some_and(|g| g.pending.contains_key(&host))
                    });
                    if let Some(pi) = gate {
                        let name = self.live[ri].record.runtime.phases[pi].name.clone();
                        self.live[ri].hosts[hi].runtime.failed_checks.clear();
                        let why = format!("pre-phase gate after '{name}': {why}");
                        self.fail_gate(ri, pi, &host, &why);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ driving

    async fn drive(&mut self) {
        self.on_time();
        for _ in 0..64 {
            let mut changed = false;
            for ri in 0..self.live.len() {
                let state = self.live[ri].record.state;
                if self.live[ri].record.is_running() {
                    changed |= self.drive_phases(ri).await;
                }
                if matches!(
                    self.live[ri].record.state,
                    RolloutState::Paused
                        | RolloutState::Rolling
                        | RolloutState::Baking
                        | RolloutState::AwaitingApproval
                ) {
                    changed |= self.drive_released(ri);
                }
                if state == RolloutState::RollingBack {
                    changed |= self.drive_rollback(ri);
                }
                if self.live[ri].record.runtime.stragglers_open {
                    self.drive_stragglers(ri);
                }
                self.refresh_display(ri);
            }
            changed |= self.start_next_queued().await;
            if !changed {
                break;
            }
        }
    }

    async fn start_next_queued(&mut self) -> bool {
        if self.live.iter().any(|l| l.record.is_active()) {
            return false;
        }
        let queued: Vec<usize> = (0..self.live.len())
            .filter(|&i| self.live[i].record.state == RolloutState::Detected)
            .collect();
        let Some(&newest) = queued.last() else {
            return false;
        };
        let newest_id = self.live[newest].record.id.clone();
        for &i in &queued[..queued.len() - 1] {
            self.finish(
                i,
                RolloutState::Aborted,
                "controller",
                &format!("superseded by {newest_id}"),
            );
        }
        if let Err(e) = self.freeze(newest).await {
            tracing::error!(rollout = %newest_id, error = %e, "a queued rollout could not start");
            self.finish(
                newest,
                RolloutState::Aborted,
                "controller",
                &format!("could not start: {e}"),
            );
        }
        true
    }

    /// Advances the current phase as far as it can go. Returns whether
    /// anything changed that warrants another pass.
    async fn drive_phases(&mut self, ri: usize) -> bool {
        let mut changed = false;
        loop {
            let p = self.live[ri].record.runtime.current_phase;
            let real = self.live[ri]
                .record
                .runtime
                .phases
                .iter()
                .filter(|x| !x.virtual_stage)
                .count();
            if p >= real {
                self.complete(ri).await;
                return true;
            }
            if let Some(gate) = &self.live[ri].record.runtime.phases[p].gate {
                if !gate.pending.is_empty() {
                    return changed;
                }
                let name = self.live[ri].record.runtime.phases[p].name.clone();
                self.live[ri].record.runtime.phases[p].state = PhaseState::Done;
                self.live[ri].record.runtime.current_phase += 1;
                self.live[ri].dirty = true;
                self.live[ri].phase_spans.remove(&p);
                let id = self.live[ri].record.id.clone();
                let trace = self.live[ri].record.runtime.trace_id.clone();
                self.audit(
                    "controller",
                    "rollout.phase.done",
                    &id,
                    format!("phase '{name}' done; pre-phase gate passed"),
                    serde_json::json!({"phase": name}),
                    trace,
                );
                changed = true;
                continue;
            }
            if !self.live[ri].record.is_running() {
                return changed;
            }
            // A standing failure-budget breach (failures charged once their
            // phase started, or left after an operator acted on another
            // host) stops the phase before anything else starts.
            if let Some((bp, failures, _)) = self.breach(ri) {
                self.on_breach(ri, bp, failures, None);
                return true;
            }
            let hosts = self.live[ri].phase_hosts(p);
            let cap = self.live[ri].record.runtime.phases[p]
                .parallel
                .resolve(hosts.len());
            let mut inflight = hosts
                .iter()
                .filter(|&&i| self.live[ri].hosts[i].state.in_flight())
                .count();
            let busy = self.busy_elsewhere(ri);
            for &hi in &hosts {
                if inflight >= cap {
                    break;
                }
                let h = &self.live[ri].hosts[hi];
                if h.state == HostPhase::Planned
                    && self.eligible(&h.host)
                    && !busy.contains(&h.host)
                    && self.approved(ri, hi)
                {
                    self.start_baseline(ri, hi);
                    inflight += 1;
                    changed = true;
                }
            }
            // The phase is done when its hosts, and hosts of earlier phases an
            // operator retried, have finished (or missed it).
            let upto: Vec<usize> = (0..self.live[ri].hosts.len())
                .filter(|&i| {
                    self.live[ri].hosts[i]
                        .runtime
                        .phase_index
                        .is_some_and(|q| q <= p)
                })
                .collect();
            let done = upto.iter().all(|&i| {
                let h = &self.live[ri].hosts[i];
                h.state.finished()
                    || (matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
                        && self.offline_for_phase(h))
            });
            if !done {
                return changed;
            }
            for &hi in &upto {
                if matches!(
                    self.live[ri].hosts[hi].state,
                    HostPhase::PlanPending | HostPhase::Planned
                ) {
                    self.set_host_state(
                        ri,
                        hi,
                        HostPhase::Missed,
                        "offline or excluded during its phase",
                    );
                }
            }
            self.start_gate(ri, p);
            changed = true;
        }
    }

    /// Starts hosts an operator retried or continued, in whichever phase
    /// (the current phase's are also started by [`Controller::drive_phases`]),
    /// including while the rollout is paused by a failure.
    fn drive_released(&mut self, ri: usize) -> bool {
        let mut changed = false;
        let busy = self.busy_elsewhere(ri);
        for hi in 0..self.live[ri].hosts.len() {
            let h = &self.live[ri].hosts[hi];
            if !h.runtime.released
                || busy.contains(&h.host)
                || h.state != HostPhase::Planned
                || self.is_virtual(ri, hi)
                || self.later_phase(ri, hi)
                || !self.may_apply(ri, hi)
                || !self.approved(ri, hi)
            {
                continue;
            }
            let Some(pi) = h.runtime.phase_index else {
                continue;
            };
            let hosts = self.live[ri].phase_hosts(pi);
            let cap = self.live[ri].record.runtime.phases[pi]
                .parallel
                .resolve(hosts.len());
            let inflight = hosts
                .iter()
                .filter(|&&i| self.live[ri].hosts[i].state.in_flight())
                .count();
            if inflight < cap {
                self.start_baseline(ri, hi);
                changed = true;
            }
        }
        changed
    }

    /// A host failed (or never answered) the pre-phase gate of phase `p`: it
    /// is failed with the gate's checks, so retry, continue and skip apply to
    /// it and a resume does not re-run the same gate on it, and the rollout
    /// pauses naming it.
    fn fail_gate(&mut self, ri: usize, p: usize, host: &str, why: &str) {
        self.live[ri].record.runtime.phases[p].gate = None;
        self.live[ri].dirty = true;
        if let Some(hi) = self.live[ri].find(host) {
            self.live[ri].hosts[hi].runtime.error = Some(why.to_owned());
            self.live[ri].hosts[hi].runtime.disconnected_at = None;
            self.set_host_state(ri, hi, HostPhase::Failed, why);
        }
        self.pause(ri, "gate", format!("{host}: {why}"), Some(host.to_owned()));
    }

    /// Hosts another live rollout (another rollout's phases, or an open
    /// stragglers stage) is working on: they are not started here until it
    /// is done with them.
    fn busy_elsewhere(&self, ri: usize) -> HashSet<String> {
        self.live
            .iter()
            .enumerate()
            .filter(|(rj, _)| *rj != ri)
            .flat_map(|(_, l)| {
                l.hosts
                    .iter()
                    .filter(|h| h.state.in_flight() || h.state == HostPhase::RollingBack)
                    .map(|h| h.host.clone())
            })
            .collect()
    }

    fn start_gate(&mut self, ri: usize, p: usize) {
        let targets: Vec<String> = self.live[ri]
            .hosts
            .iter()
            .filter(|h| {
                h.state == HostPhase::Healthy && h.runtime.phase_index.is_some_and(|i| i <= p)
            })
            .map(|h| h.host.clone())
            .collect();
        let mut gate = GateRuntime::default();
        let tp = traceparent_of(&self.live[ri].span).unwrap_or_default();
        let now = self.now();
        for host in targets {
            let rid = new_id();
            gate.pending.insert(host.clone(), rid.clone());
            if self.connected(&host) {
                self.send(
                    &host,
                    server_message::Msg::Health(RunHealth {
                        request_id: rid,
                        scope: HealthScope::Rollout as i32,
                        modules: vec![],
                        until: None,
                        traceparent: tp.clone(),
                    }),
                );
            } else if let Some(hi) = self.live[ri].find(&host) {
                self.live[ri].hosts[hi]
                    .runtime
                    .disconnected_at
                    .get_or_insert(now);
                self.touch_host(ri, hi);
            }
        }
        let phase = &mut self.live[ri].record.runtime.phases[p];
        phase.state = PhaseState::Baking;
        phase.gate = Some(gate);
        self.live[ri].dirty = true;
    }

    async fn complete(&mut self, ri: usize) {
        let revision = self.live[ri].record.revision.clone();
        let id = self.live[ri].record.id.clone();
        self.finish(
            ri,
            RolloutState::Completed,
            "controller",
            "every phase baked and passed its gate",
        );
        self.live[ri].record.runtime.end_reason = None;
        self.stable = Some(revision.clone());
        self.stable_dirty = true;
        let trace = self.live[ri].record.runtime.trace_id.clone();
        self.audit(
            "controller",
            "revision.promoted",
            &id,
            format!("revision {} is now stable", short(&revision)),
            serde_json::json!({"revision": revision}),
            trace,
        );

        // Close older stragglers stages, carrying their unfinished hosts.
        let mut carried: Vec<String> = Vec::new();
        for rj in 0..self.live.len() {
            if rj == ri || !self.live[rj].record.runtime.stragglers_open {
                continue;
            }
            if let Some(si) = self.live[rj].stragglers_index() {
                for h in self.live[rj].phase_hosts(si) {
                    let host = &self.live[rj].hosts[h];
                    if !matches!(
                        host.state,
                        HostPhase::Healthy
                            | HostPhase::Skipped
                            | HostPhase::Failed
                            | HostPhase::RolledBack
                    ) {
                        carried.push(host.host.clone());
                    }
                }
                self.live[rj].record.runtime.phases[si].state = PhaseState::Done;
            }
            self.live[rj].record.runtime.stragglers_open = false;
            self.live[rj].dirty = true;
            let rid = self.live[rj].record.id.clone();
            self.audit(
                "controller",
                "rollout.stragglers.closed",
                &rid,
                format!("stragglers stage closed; carried to {id}"),
                serde_json::json!({"carried": carried}),
                None,
            );
        }

        // Open this rollout's stragglers stage.
        let policy = self.live[ri].record.policy.clone();
        let si = self.live[ri].record.runtime.phases.len();
        self.live[ri].record.runtime.phases.push(PhaseRuntime {
            name: STRAGGLERS.into(),
            state: PhaseState::Open,
            parallel: policy.stragglers.parallel,
            approval: policy.stragglers.approval,
            bake_seconds: policy.health.bake.as_secs(),
            max_failures: buckle_core::model::Parallelism::Count(0),
            virtual_stage: true,
            accepted_failures: 0,
            started: 0,
            gate: None,
            bake_until: None,
        });
        self.live[ri].record.runtime.current_phase = si;
        self.live[ri].record.runtime.stragglers_open = true;
        self.live[ri].dirty = true;
        for hi in 0..self.live[ri].hosts.len() {
            let h = &self.live[ri].hosts[hi];
            // Hosts not updated during their phase: missed, still planning,
            // held at baseline, or released from a baseline hold by an
            // operator's skip (`baseline: skip` excludes a host for good).
            let not_updated = matches!(
                h.state,
                HostPhase::Missed | HostPhase::PlanPending | HostPhase::Planned | HostPhase::Held
            ) || (h.state == HostPhase::Skipped
                && h.runtime.held_at_baseline
                && !h.runtime.applied);
            if not_updated {
                self.move_to_stragglers(ri, hi, si);
            }
        }
        // Hosts enrolled since the freeze, and carried stragglers.
        let candidates = self
            .store
            .call(|db| {
                let mut out = Vec::new();
                for h in db.hosts().list()? {
                    let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                    out.push((h, facts.unwrap_or(serde_json::Value::Null)));
                }
                Ok(out)
            })
            .await
            .unwrap_or_default();
        for (host, facts) in candidates {
            let carried_here = carried.contains(&host.id);
            if self.live[ri].find(&host.id).is_some() || host.status != HostStatus::Active {
                continue;
            }
            if !carried_here {
                if self.live[ri].record.runtime.untargeted.contains(&host.id)
                    || host.applied_revision.as_deref() == Some(revision.as_str())
                {
                    continue;
                }
                let ctx =
                    crate::fleet::record_context(&host, &facts, &self.live[ri].revision.fleet);
                if crate::fleet::phase_for(&ctx, &crate::fleet::effective_phases(&policy)).is_none()
                {
                    continue;
                }
            }
            self.add_straggler(ri, &host.id, si);
        }
    }

    fn move_to_stragglers(&mut self, ri: usize, hi: usize, si: usize) {
        {
            let h = &mut self.live[ri].hosts[hi];
            h.phase = STRAGGLERS.into();
            h.runtime.phase_index = Some(si);
            h.plan_digest = None;
            h.wave = None;
            h.runtime.held_at_baseline = false;
            h.runtime.error = None;
            h.runtime.failed_checks.clear();
        }
        self.live[ri].hosts[hi].state = HostPhase::Missed;
        self.set_host_state(
            ri,
            hi,
            HostPhase::PlanPending,
            "moved to the stragglers stage",
        );
        self.request_plan(ri, hi);
    }

    fn add_straggler(&mut self, ri: usize, host: &str, si: usize) {
        let now = self.now();
        let revision = self.live[ri].record.revision.clone();
        let id = self.live[ri].record.id.clone();
        self.live[ri].hosts.push(RolloutHostRecord {
            rollout: id.clone(),
            host: host.to_owned(),
            phase: STRAGGLERS.into(),
            wave: None,
            state: HostPhase::PlanPending,
            plan_digest: None,
            plan: None,
            result: None,
            health: None,
            created_at: now,
            updated_at: now,
            runtime: HostRuntime {
                revision,
                phase_index: Some(si),
                ..Default::default()
            },
        });
        let hi = self.live[ri].hosts.len() - 1;
        self.live[ri].dirty_hosts.insert(hi);
        let trace = self.live[ri].record.runtime.trace_id.clone();
        self.audit(
            "controller",
            "rollout.stragglers.added",
            &id,
            format!("{host} joined the stragglers stage"),
            serde_json::json!({"host": host, "phase": STRAGGLERS}),
            trace,
        );
        self.request_plan(ri, hi);
    }

    async fn maybe_add_straggler(&mut self, host: &str, applied: Option<&str>) {
        let Some(ri) = self
            .live
            .iter()
            .position(|l| l.record.runtime.stragglers_open)
        else {
            return;
        };
        if self.live[ri].find(host).is_some() || self.excluded.contains(host) {
            return;
        }
        if applied == Some(self.live[ri].record.revision.as_str()) {
            return;
        }
        if self.live[ri]
            .record
            .runtime
            .untargeted
            .iter()
            .any(|h| h == host)
        {
            return;
        }
        let h = host.to_owned();
        let found = self
            .store
            .call(move |db| {
                let Some(rec) = db.hosts().get(&h)? else {
                    return Ok(None);
                };
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                Ok(Some((rec, facts.unwrap_or(serde_json::Value::Null))))
            })
            .await
            .ok()
            .flatten();
        let Some((record, facts)) = found else { return };
        if record.status != HostStatus::Active {
            return;
        }
        let ctx = crate::fleet::record_context(&record, &facts, &self.live[ri].revision.fleet);
        if crate::fleet::phase_for(
            &ctx,
            &crate::fleet::effective_phases(&self.live[ri].record.policy),
        )
        .is_none()
        {
            return;
        }
        if let Some(si) = self.live[ri].stragglers_index() {
            self.add_straggler(ri, host, si);
        }
    }

    fn drive_stragglers(&mut self, ri: usize) {
        let Some(si) = self.live[ri].stragglers_index() else {
            return;
        };
        let hosts = self.live[ri].phase_hosts(si);
        let cap = self.live[ri].record.runtime.phases[si]
            .parallel
            .resolve(hosts.len());
        let busy: HashSet<String> = self
            .live
            .iter()
            .filter(|l| l.record.is_active())
            .flat_map(|l| l.hosts.iter().map(|h| h.host.clone()))
            .collect();
        let mut inflight = hosts
            .iter()
            .filter(|&&i| self.live[ri].hosts[i].state.in_flight())
            .count();
        for &hi in &hosts {
            if inflight >= cap {
                break;
            }
            let h = &self.live[ri].hosts[hi];
            if h.state == HostPhase::Planned
                && self.eligible(&h.host)
                && !busy.contains(&h.host)
                && self.approved(ri, hi)
            {
                self.start_baseline(ri, hi);
                inflight += 1;
            }
        }
    }

    fn drive_rollback(&mut self, ri: usize) -> bool {
        let mut changed = false;
        loop {
            let Some(p) = self.live[ri].record.runtime.rollback_phase else {
                self.finish(
                    ri,
                    RolloutState::RolledBack,
                    "controller",
                    "previous stable revision applied to every updated host",
                );
                self.live[ri].record.runtime.end_reason = None;
                return true;
            };
            let targets: Vec<usize> = self.live[ri]
                .phase_hosts(p)
                .into_iter()
                .filter(|&i| rollback_target(&self.live[ri].hosts[i]))
                .collect();
            for &hi in &targets {
                let state = self.live[ri].hosts[hi].state;
                let host = self.live[ri].hosts[hi].host.clone();
                let attempted = matches!(state, HostPhase::RollingBack | HostPhase::RolledBack)
                    || (state == HostPhase::Failed
                        && self.live[ri].hosts[hi]
                            .runtime
                            .error
                            .as_deref()
                            .is_some_and(|e| e.starts_with("rollback failed")));
                if !attempted && self.connected(&host) {
                    self.set_host_state(ri, hi, HostPhase::RollingBack, "rolling back");
                    self.send_rollback(ri, hi);
                    changed = true;
                } else if !attempted {
                    // Wait for it; the disconnect timeout fails it eventually.
                    self.live[ri].hosts[hi].state = HostPhase::RollingBack;
                    self.live[ri].hosts[hi]
                        .runtime
                        .disconnected_at
                        .get_or_insert(self.clock.now());
                    self.touch_host(ri, hi);
                }
            }
            let waiting = targets
                .iter()
                .any(|&i| self.live[ri].hosts[i].state == HostPhase::RollingBack);
            if waiting {
                return changed;
            }
            self.live[ri].record.runtime.rollback_phase = (0..p).rev().find(|&q| {
                self.live[ri]
                    .phase_hosts(q)
                    .iter()
                    .any(|&i| rollback_target(&self.live[ri].hosts[i]))
            });
            self.live[ri].dirty = true;
            changed = true;
        }
    }

    fn refresh_display(&mut self, ri: usize) {
        let live = &mut self.live[ri];
        let cur = live.record.runtime.current_phase;
        let running = live.record.is_running();
        let paused = live.record.state == RolloutState::Paused;
        let n_phases = live.record.runtime.phases.len();
        let mut display = None;
        for i in 0..n_phases {
            let hosts: Vec<usize> = (0..live.hosts.len())
                .filter(|&h| live.hosts[h].runtime.phase_index == Some(i))
                .collect();
            let phase = &live.record.runtime.phases[i];
            if phase.virtual_stage {
                continue;
            }
            let state = if live.record.is_terminal() {
                if i < cur || phase.state == PhaseState::Done {
                    PhaseState::Done
                } else {
                    PhaseState::Skipped
                }
            } else if i < cur {
                PhaseState::Done
            } else if i > cur {
                PhaseState::Pending
            } else if paused {
                PhaseState::Paused
            } else if phase.gate.is_some() {
                PhaseState::Baking
            } else {
                let any_flight = hosts.iter().any(|&h| live.hosts[h].state.in_flight());
                let all_baking = any_flight
                    && hosts
                        .iter()
                        .filter(|&&h| live.hosts[h].state.in_flight())
                        .all(|&h| live.hosts[h].state == HostPhase::Baking);
                let waiting_approval = !any_flight
                    && hosts.iter().any(|&h| {
                        live.hosts[h].state == HostPhase::Planned
                            && view::needs_approval(Some(phase), &live.hosts[h], &live.approvals)
                    });
                if waiting_approval {
                    PhaseState::AwaitingApproval
                } else if all_baking {
                    PhaseState::Baking
                } else {
                    PhaseState::Running
                }
            };
            if i == cur && running {
                let all_planning = !hosts.is_empty()
                    && hosts
                        .iter()
                        .all(|&h| live.hosts[h].state == HostPhase::PlanPending);
                display = Some(match state {
                    _ if all_planning => RolloutState::Planning,
                    PhaseState::AwaitingApproval => RolloutState::AwaitingApproval,
                    PhaseState::Baking => RolloutState::Baking,
                    _ => RolloutState::Rolling,
                });
            }
            let bake_until = hosts
                .iter()
                .filter_map(|&h| live.hosts[h].runtime.bake_until)
                .max();
            let phase = &mut live.record.runtime.phases[i];
            if phase.state != state || phase.bake_until != bake_until {
                phase.state = state;
                phase.bake_until = bake_until;
                live.dirty = true;
            }
        }
        if let Some(d) = display
            && d != live.record.state
        {
            self.set_rollout_state(ri, d, "controller", "");
        }
    }

    // ------------------------------------------------------------ persistence

    /// Writes the checkpoint (dirty rollouts and hosts, the stable revision,
    /// audit rows) in one transaction, then publishes live events and sends
    /// the queued messages. Returns whether the checkpoint was written.
    ///
    /// When the transaction fails nothing is lost or sent ahead of its
    /// state: the audit rows and the outbox are kept, finished rollouts stay
    /// live, and a retry is armed with a backoff.
    async fn flush(&mut self) -> bool {
        let mut rollouts = Vec::new();
        let mut hosts = Vec::new();
        for l in &self.live {
            if l.dirty {
                rollouts.push(l.record.clone());
            }
            for &hi in &l.dirty_hosts {
                if let Some(h) = l.hosts.get(hi) {
                    hosts.push(h.clone());
                }
            }
        }
        let audits = std::mem::take(&mut self.audits);
        let stable = self.stable_dirty.then(|| self.stable.clone()).flatten();
        let cleared = self.pending_clear.clone();
        if !(rollouts.is_empty()
            && hosts.is_empty()
            && audits.is_empty()
            && stable.is_none()
            && cleared.is_empty())
        {
            let (r2, h2, a2) = (rollouts.clone(), hosts.clone(), audits.clone());
            let result = self
                .store
                .transaction(move |db| {
                    for r in &r2 {
                        db.rollouts().save(r)?;
                    }
                    for h in &h2 {
                        db.rollouts().save_host(h)?;
                    }
                    if let Some(s) = &stable {
                        db.kv().set(keys::STABLE_REVISION, s)?;
                    }
                    for key in &cleared {
                        db.kv().delete(key)?;
                    }
                    let mut entries = Vec::new();
                    for a in &a2 {
                        entries.push(db.audit().add(a)?);
                    }
                    Ok(entries)
                })
                .await;
            match result {
                Ok(entries) => {
                    self.flush_retry = None;
                    self.flush_failures = 0;
                    self.pending_clear.clear();
                    self.stable_dirty = false;
                    for l in &mut self.live {
                        l.dirty = false;
                        l.dirty_hosts.clear();
                    }
                    for r in &rollouts {
                        if let Some(l) = self.live.iter().find(|l| l.record.id == r.id) {
                            self.events.publish(LiveEvent::RolloutChanged {
                                rollout: view::summary(&l.record, &l.hosts),
                            });
                        }
                    }
                    for h in &hosts {
                        if let Some(l) = self.live.iter().find(|l| l.record.id == h.rollout) {
                            self.events.publish(LiveEvent::RolloutHostChanged {
                                rollout_id: h.rollout.clone(),
                                host: view::host(&l.record, h, &l.approvals),
                            });
                        }
                    }
                    for e in entries {
                        self.events.publish(LiveEvent::Audit {
                            entry: buckle_api::AuditEntryRef {
                                id: e.id,
                                at: e.at,
                                action: e.action,
                                message: e.message,
                            },
                        });
                    }
                }
                Err(e) => {
                    // Keep the audit rows (ahead of any recorded since) and
                    // the outbox: nothing is sent before its state is saved.
                    let mut kept = audits;
                    kept.append(&mut self.audits);
                    self.audits = kept;
                    self.flush_failures = self.flush_failures.saturating_add(1);
                    let backoff = 1i64 << self.flush_failures.min(5);
                    self.flush_retry = Some(self.now() + chrono::Duration::seconds(backoff));
                    tracing::error!(error = %e, failures = self.flush_failures, retry_in_s = backoff, "the rollout checkpoint could not be written; will retry");
                    return false;
                }
            }
        }
        let outbox = std::mem::take(&mut self.outbox);
        let mut undeliverable: BTreeSet<String> = BTreeSet::new();
        for out in outbox {
            match out {
                Out::Msg(host, m) => {
                    if !self.link.send(&host, m) {
                        undeliverable.insert(host);
                    }
                }
                Out::AssignAndPlan {
                    host,
                    revision,
                    stable,
                } => {
                    if !self.assign_and_plan(&host, &revision, stable).await {
                        undeliverable.insert(host);
                    }
                }
            }
        }
        // A message that could not be delivered means the session is gone:
        // treat it as a disconnect so the disconnect timeout runs and the
        // reconnect re-issues what was lost.
        for host in undeliverable {
            tracing::debug!(host = %host, "a message could not be delivered; treating the host as disconnected");
            self.on_disconnected(&host);
        }
        // The reactions to these durable reports are saved: they may be acked.
        for done in std::mem::take(&mut self.ack_waiters) {
            let _ = done.send(true);
        }
        // Forget finished rollouts once their final state is written (they
        // stay in the store).
        self.live.retain(|l| {
            !l.record.is_terminal()
                || l.record.runtime.stragglers_open
                || l.dirty
                || !l.dirty_hosts.is_empty()
        });
        true
    }

    /// Sends Assign and RequestPlan; `false` when the host has no session.
    async fn assign_and_plan(&mut self, host: &str, revision: &str, stable: bool) -> bool {
        let rev = match self.revisions.load(revision).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(revision = %revision, error = %e, "the revision to assign could not be loaded");
                return true;
            }
        };
        let h = host.to_owned();
        let rid = revision.to_owned();
        let found = self
            .store
            .call(move |db| {
                let rec = db.hosts().get(&h)?;
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                let digest = db.revisions().get(&rid)?.and_then(|r| r.tarball_digest);
                Ok((rec, facts.unwrap_or(serde_json::Value::Null), digest))
            })
            .await;
        let Ok((Some(record), facts, tree_digest)) = found else {
            return true;
        };
        let ctx = crate::fleet::record_context(&record, &facts, &rev.fleet);
        let secrets = self.secrets.for_host(&rev, &ctx).await;
        let assigned = self.link.send(
            host,
            msg(server_message::Msg::Assign(Assign {
                revision: revision.to_owned(),
                stable,
                secrets: Some(secrets),
                tree_digest: tree_digest.unwrap_or_default(),
            })),
        );
        let planned = self.link.send(
            host,
            msg(server_message::Msg::Plan(RequestPlan {
                revision: revision.to_owned(),
                select: PLAN_SELECT.to_owned(),
            })),
        );
        assigned && planned
    }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(8)]
}

/// What is stored as the host row's `result`.
#[derive(serde::Serialize)]
struct ResultView {
    revision: String,
    plan_digest: String,
    succeeded: bool,
    changes: u32,
    removals: u32,
    applied_revision: String,
    failed_module: String,
    rolled_back_modules: Vec<String>,
    not_started_modules: Vec<String>,
    error: String,
    failed_checks: Vec<String>,
    outcome: Option<serde_json::Value>,
}

impl From<&ApplyResult> for ResultView {
    fn from(r: &ApplyResult) -> Self {
        Self {
            revision: r.revision.clone(),
            plan_digest: r.plan_digest.clone(),
            succeeded: r.succeeded,
            changes: r.changes,
            removals: r.removals,
            applied_revision: r.applied_revision.clone(),
            failed_module: r.failed_module.clone(),
            rolled_back_modules: r.rolled_back_modules.clone(),
            not_started_modules: r.not_started_modules.clone(),
            error: r.error.clone(),
            failed_checks: r.failed_checks.clone(),
            outcome: serde_json::from_slice(&r.outcome_json).ok(),
        }
    }
}

/// What is stored as the host row's `result` after a rollback the host ran
/// on its own: the failure that triggered it and the rollback's own result.
#[derive(serde::Serialize)]
struct RollbackView {
    kind: &'static str,
    from_revision: String,
    to_revision: String,
    modules: Vec<String>,
    reason: String,
    evidence: Vec<serde_json::Value>,
    succeeded: bool,
    rollback_error: String,
    rollback_outcome: Option<serde_json::Value>,
}

impl From<&LocalRollback> for RollbackView {
    fn from(r: &LocalRollback) -> Self {
        Self {
            kind: if r.modules.is_empty() {
                "local_rollback"
            } else {
                "module_rollback"
            },
            from_revision: r.from_revision.clone(),
            to_revision: r.to_revision.clone(),
            modules: r.modules.clone(),
            reason: r.reason.clone(),
            evidence: r.evidence.iter().map(health_result_json).collect(),
            succeeded: r.succeeded,
            rollback_error: r.rollback_error.clone(),
            rollback_outcome: serde_json::from_slice(&r.rollback_outcome_json).ok(),
        }
    }
}

fn health_result_json(x: &pb::HealthResult) -> serde_json::Value {
    serde_json::json!({
        "module": x.module,
        "check": x.check,
        "kind": x.kind,
        "required": x.required,
        "passed": x.passed,
        "unknown": x.unknown,
        "reason": x.reason,
    })
}

/// What is stored as the host row's `health`.
#[derive(serde::Serialize)]
struct HealthView {
    scope: i32,
    results: Vec<serde_json::Value>,
}

impl From<&HealthReport> for HealthView {
    fn from(r: &HealthReport) -> Self {
        Self {
            scope: r.scope,
            results: r.results.iter().map(health_result_json).collect(),
        }
    }
}
