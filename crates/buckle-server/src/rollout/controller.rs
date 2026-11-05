//! The controller actor. One task owns all live rollout state; every input
//! (agent message, connect, disconnect, operator command, timer) is handled
//! to completion, checkpointed to the store in one transaction with its
//! audit rows, broadcast as live events, and only then are the resulting
//! messages sent to hosts.

use super::bake::{self, BakeWindow};
use super::link::HostLink;
use super::model::{
    GateRuntime, HostPhase, HostRuntime, PauseInfo, PhaseRuntime, RebootWait, RolloutAction,
    RolloutHostRecord, RolloutRecord, RolloutRuntime, STRAGGLERS,
};
use super::stages;
use super::view;
use crate::clock::Clock;
use crate::events::EventBus;
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{ApprovalRecord, NewAudit, Store, kv::keys};
use buckle_api::{HostStatus, LiveEvent, PhaseState, RevisionStatus, RolloutState, TargetKind};
use buckle_core::engine::Revision;
use buckle_core::model::{ApprovalPolicy, BaselinePolicy, FailurePolicy, SupersedePolicy};
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
/// How often an agent may refuse the approved Apply as stale (its fresh plan
/// differs) before the host fails: a plan that changes on every attempt
/// would otherwise cycle planned → applying → planned forever.
pub(super) const MAX_STALE_REFUSALS: u32 = 3;

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
    /// Optional behaviours the agent announced (`Hello.capabilities`).
    pub capabilities: Vec<String>,
    /// The boot the agent is running in (`Hello.boot_id`), when it said.
    pub boot_id: Option<String>,
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
    /// Re-plan a host whose plan has not been applied yet (still to plan,
    /// planned), and re-apply one that failed, was held, skipped or missed.
    Retry {
        rollout: String,
        host: String,
        note: String,
    },
    /// Re-plan and re-apply every failed host of the rollout.
    RetryFailed {
        rollout: String,
        note: String,
    },
    /// End the bake of a baking host (`host`), or of every baking host,
    /// now: it counts as baked.
    SkipBake {
        rollout: String,
        host: Option<String>,
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
    /// Start an action rollout: run `action` on each of `targets` (target →
    /// the revision it runs, which declares the action) through the stable
    /// revision's phases; replies with the rollout's id.
    StartAction {
        action: RolloutAction,
        targets: BTreeMap<String, String>,
        note: String,
    },
    /// Run an action of `module` (a cluster's: a workload) on `target`
    /// from `revision`, its applied revision; replies with the run's id.
    RunAction {
        target: String,
        module: String,
        action: String,
        revision: String,
        note: String,
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
            Command::RetryFailed { .. } => "rollout.retry",
            Command::SkipBake { host: None, .. } => "rollout.skip_bake",
            Command::SkipBake { host: Some(_), .. } => "rollout.host.skip_bake",
            Command::Skip { .. } => "rollout.host.skip",
            Command::Continue { .. } => "rollout.host.continue",
            Command::Force { .. } => "rollout.host.force",
            Command::Reject { .. } => "rollout.reject",
            Command::RunAction { .. } => "host.action.run",
            Command::StartAction { .. } => "rollout.action.start",
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
fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

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

    /// The row a report from `host` is about: the host's own, or, when the
    /// report names a `target`, that cluster's row while `host` is its
    /// executor (a report from an executor since replaced is not its).
    fn find_target(&self, host: &str, target: &str) -> Option<usize> {
        if target.is_empty() {
            return self.find(host);
        }
        self.hosts.iter().position(|h| {
            h.host == target
                && h.runtime.kind == TargetKind::Cluster
                && h.runtime.executor.as_deref() == Some(host)
        })
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
    /// Boxed: a `ServerMessage` is large next to the other variant.
    Msg(String, Box<ServerMessage>),
    AssignAndPlan {
        /// The session the messages travel over: the host, or a cluster's executor.
        session: String,
        /// The target the plan is for; empty for the host itself.
        target: String,
        revision: String,
        stable: bool,
    },
    /// Work for a cluster over its executor's session, sent behind a
    /// targeted Assign of the revision it concerns: the executor keeps
    /// nothing about the cluster between instructions.
    Targeted {
        session: String,
        target: String,
        msg: Box<ServerMessage>,
    },
}

/// A host agent speaks for clusters only once it announces it runs a
/// pipeline per target (`Hello.capabilities`); older agents are never handed
/// a cluster's plan.
pub const TARGETS_CAPABILITY: &str = "targets";

/// The outcome of looking for a cluster's executor.
enum Pick {
    /// This host speaks for the cluster.
    Executor(String),
    /// Hosts could, but every one is mid-wave: wait for them.
    Busy,
    /// No connected, capable host matches the cluster's `executors`.
    None,
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
    /// Action runs to store with the next checkpoint, before the messages
    /// that start them are sent.
    new_runs: Vec<crate::store::RunRecord>,
    stable: Option<String>,
    stable_dirty: bool,
    excluded: HashSet<String>,
    /// Hosts this controller has seen connect and not yet seen disconnect.
    /// A dropped session leaves the link before its disconnect reaches the
    /// controller; until then the host is not taken for offline.
    online: HashSet<String>,
    /// What each host's agent announced in its last Hello (`force_plan`, …).
    capabilities: HashMap<String, Vec<String>>,
    /// The boot each host's current (or last) session runs in.
    boot_ids: HashMap<String, String>,
    /// Each cluster's last executor (kv `executor/<cluster>`), so a pick
    /// stays with the same host while it qualifies.
    executors: BTreeMap<String, String>,
    /// Clusters whose executor changed since the last checkpoint.
    executors_dirty: BTreeSet<String>,
    /// Each cluster target's spec as last loaded (its `executors` and
    /// `nodes` selectors), for the executor pick and the stage layout.
    clusters: HashMap<String, buckle_api::ClusterSpec>,
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

/// Names `target` on a message bound for a cluster's executor. Empty names
/// the host itself and leaves the message as it is.
fn stamp_target(mut m: ServerMessage, target: &str) -> ServerMessage {
    if target.is_empty() {
        return m;
    }
    match &mut m.msg {
        Some(server_message::Msg::Assign(a)) => a.target = target.to_owned(),
        Some(server_message::Msg::Plan(p)) => p.target = target.to_owned(),
        Some(server_message::Msg::Apply(a)) => a.target = target.to_owned(),
        Some(server_message::Msg::Rollback(r)) => r.target = target.to_owned(),
        Some(server_message::Msg::Health(h)) => h.target = target.to_owned(),
        Some(server_message::Msg::Action(a)) => a.target = target.to_owned(),
        _ => {}
    }
    m
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
/// reconnect grace. A rebooting host is meant to be away: its reboot
/// timeout bounds the wait instead.
fn awaits_reconnect(state: HostPhase) -> bool {
    state != HostPhase::Rebooting
        && (state.in_flight()
            || matches!(
                state,
                HostPhase::RollingBack | HostPhase::PlanPending | HostPhase::Planned
            ))
}

/// The `bake` of row `h`'s phase, in seconds.
fn phase_bake(phases: &[PhaseRuntime], h: &RolloutHostRecord) -> u64 {
    h.runtime
        .phase_index
        .and_then(|i| phases.get(i))
        .map(|p| p.bake_seconds)
        .unwrap_or(0)
}

/// Row `h`'s bake window before any reboot: what its step resolved to,
/// else its phase's.
fn row_bake(phases: &[PhaseRuntime], h: &RolloutHostRecord) -> u64 {
    h.runtime
        .bake
        .as_ref()
        .map_or_else(|| phase_bake(phases, h), |b| b.seconds)
}

/// The shortest bake after a reboot, so that services coming up get checked.
const MIN_POST_REBOOT_BAKE: u64 = 60;

fn duration_of(d: Option<&prost_types::Duration>) -> chrono::Duration {
    d.map(|d| chrono::Duration::seconds(d.seconds) + chrono::Duration::nanoseconds(d.nanos as i64))
        .unwrap_or_default()
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
                let executors: BTreeMap<String, String> = db
                    .kv()
                    .with_prefix(keys::EXECUTOR_PREFIX)?
                    .into_iter()
                    .map(|(k, v)| (k[keys::EXECUTOR_PREFIX.len()..].to_owned(), v))
                    .collect();
                Ok((
                    out,
                    db.kv().get(keys::STABLE_REVISION)?,
                    (excluded, seen, pending, executors),
                ))
            })
            .await?;
        let (excluded, host_seen, pending, executors) = excluded;
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
            new_runs: Vec::new(),
            audits: Vec::new(),
            stable,
            stable_dirty: false,
            excluded,
            online: HashSet::new(),
            capabilities: HashMap::new(),
            boot_ids: HashMap::new(),
            executors,
            executors_dirty: BTreeSet::new(),
            clusters: HashMap::new(),
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

    /// The session row `hi`'s messages travel over: the host itself, or the
    /// executor picked for a cluster (`None` while it has none).
    fn row_session(&self, ri: usize, hi: usize) -> Option<String> {
        let h = &self.live[ri].hosts[hi];
        match h.runtime.kind {
            TargetKind::Host => Some(h.host.clone()),
            TargetKind::Cluster => h.runtime.executor.clone(),
        }
    }

    /// Whether row `hi` can be reached now.
    fn row_connected(&self, ri: usize, hi: usize) -> bool {
        self.row_session(ri, hi).is_some_and(|s| self.connected(&s))
    }

    /// Whether row `hi` may be sent work: not excluded, and its session is
    /// connected and not excluded either.
    fn row_eligible(&self, ri: usize, hi: usize) -> bool {
        !self.excluded.contains(&self.live[ri].hosts[hi].host)
            && self.row_session(ri, hi).is_some_and(|s| self.eligible(&s))
    }

    /// Whether row `hi`'s stage is open: its phase is the current one (or
    /// the stragglers stage, which has no stages) and the phase has reached
    /// the stage of the row's current step.
    fn stage_open(&self, ri: usize, hi: usize) -> bool {
        let live = &self.live[ri];
        let h = &live.hosts[hi];
        let Some(pi) = h.runtime.phase_index else {
            return true;
        };
        let Some(phase) = live.record.runtime.phases.get(pi) else {
            return true;
        };
        phase.virtual_stage
            || (pi == live.record.runtime.current_phase && h.runtime.stage <= phase.open_stage)
    }

    /// Whether a target still to plan or apply counts as missing its phase:
    /// excluded, or known to be unreachable and not inside a reconnect grace
    /// (after a restart every host is disconnected until its agent comes
    /// back; the disconnect clock started by [`Controller::load`] bounds the
    /// wait). A host whose session has left the link but whose disconnect
    /// has not reached the controller yet is not offline: the disconnect
    /// starts its grace when it arrives. A cluster is unreachable when it
    /// has no executor, which only counts once its stage has opened and the
    /// grace that started then has passed.
    fn offline_for_phase(&self, ri: usize, hi: usize) -> bool {
        let h = &self.live[ri].hosts[hi];
        if self.excluded.contains(&h.host) {
            return true;
        }
        if h.runtime.disconnected_at.is_some() {
            return false;
        }
        if h.runtime.kind == TargetKind::Cluster && !self.stage_open(ri, hi) {
            return false;
        }
        match self.row_session(ri, hi) {
            None => true,
            Some(s) => !self.connected(&s) && !self.online.contains(&s),
        }
    }

    /// Whether row `hi` is done with its current stage as far as the stage's
    /// completion is concerned: finished, or still to plan or apply but
    /// unreachable (it will miss the phase).
    fn stage_done(&self, ri: usize, hi: usize) -> bool {
        let h = &self.live[ri].hosts[hi];
        h.state.finished()
            || (matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
                && self.offline_for_phase(ri, hi))
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
        open && self.row_eligible(ri, hi)
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

    /// Sends `m` to row `hi`: over its own session for a host, over its
    /// executor's with the cluster named as the target for a cluster.
    /// Nothing is sent to a cluster without an executor.
    fn send_to(&mut self, ri: usize, hi: usize, m: server_message::Msg) {
        let Some(session) = self.row_session(ri, hi) else {
            return;
        };
        let target = self.row_target(ri, hi);
        let m = Box::new(stamp_target(msg(m), &target));
        self.outbox.push(if target.is_empty() {
            Out::Msg(session, m)
        } else {
            Out::Targeted {
                session,
                target,
                msg: m,
            }
        });
    }

    /// The `target` a row's messages carry: the cluster's id, or empty for
    /// a host (its own).
    fn row_target(&self, ri: usize, hi: usize) -> String {
        let h = &self.live[ri].hosts[hi];
        match h.runtime.kind {
            TargetKind::Host => String::new(),
            TargetKind::Cluster => h.host.clone(),
        }
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
                agent_message::Msg::RebootFailed(f) => self.on_reboot_failed(&host, f),
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
                        agent_message::Msg::Action(a) => self.on_action_result(&host, a),
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
        self.online.insert(c.host.clone());
        if c.new_session {
            self.capabilities
                .insert(c.host.clone(), c.capabilities.clone());
            match &c.boot_id {
                Some(boot) => self.boot_ids.insert(c.host.clone(), boot.clone()),
                None => self.boot_ids.remove(&c.host),
            };
        }
        for ri in 0..self.live.len() {
            let rows: Vec<usize> = (0..self.live[ri].hosts.len())
                .filter(|&hi| self.row_session(ri, hi).as_deref() == Some(c.host.as_str()))
                .collect();
            for hi in rows {
                self.on_row_reachable(ri, hi, &c).await;
            }
        }
        // A running rollout gets first call on a host that appears: only
        // then does the stable revision's stragglers stage consider it.
        for ri in 0..self.live.len() {
            self.admit_late_targets(ri, Some(&c.host)).await;
        }
        self.maybe_add_straggler(&c.host, c.applied_revision.as_deref())
            .await;
    }

    /// The session row `hi` travels over (its host, or its executor) is back:
    /// re-issues whatever the row waits on.
    async fn on_row_reachable(&mut self, ri: usize, hi: usize, c: &Connected) {
        let now = self.now();
        {
            self.live[ri].hosts[hi].runtime.disconnected_at = None;
            self.touch_host(ri, hi);
            if self.excluded.contains(&self.live[ri].hosts[hi].host) {
                return;
            }
            let state = self.live[ri].hosts[hi].state;
            let target = self.live[ri].hosts[hi].runtime.revision.clone();
            let active =
                self.live[ri].record.is_active() || self.live[ri].record.runtime.stragglers_open;
            if !active {
                return;
            }
            // What the Hello says about applies and revisions is about the
            // host itself; a cluster's executor says nothing about the
            // cluster, so its in-flight work is simply waited for.
            let own = self.live[ri].hosts[hi].runtime.kind == TargetKind::Host;
            match state {
                HostPhase::PlanPending => self.request_plan(ri, hi),
                HostPhase::Baseline if self.may_apply(ri, hi) => self.send_baseline(ri, hi),
                HostPhase::Baseline => {
                    self.defer_host(ri, hi, "reconnected while the rollout is not running")
                }
                // An action is not idempotent and is never re-sent: its
                // result is durable (the agent re-sends it), and one that
                // never ran fails by the apply timeout.
                HostPhase::Applying if self.live[ri].record.runtime.action.is_some() => {
                    tracing::debug!(host = %c.host, target = %self.live[ri].hosts[hi].host, "reconnected while running an action; waiting for its result");
                }
                HostPhase::Applying if !own => {
                    tracing::debug!(host = %c.host, target = %self.live[ri].hosts[hi].host, "the executor is back; waiting for the cluster's result");
                }
                HostPhase::Applying => {
                    // The agent skips an Apply whose session ended before it
                    // started, so the server re-sends it (same plan digest,
                    // same approval token) unless the host announces the
                    // apply as running or its result as still to come (the
                    // result says how it went, and whether it rebooted), or
                    // already shows the target revision (it finished: bake).
                    if !c.new_session || c.unreported_apply_revisions.contains(&target) {
                        tracing::debug!(host = %c.host, "the apply is running or its result is on its way");
                    } else if c.applied_revision.as_deref() == Some(target.as_str()) {
                        self.start_bake(ri, hi);
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
                HostPhase::Rebooting if c.new_session && own => {
                    let back = self.live[ri].hosts[hi]
                        .runtime
                        .reboot
                        .as_ref()
                        .is_none_or(|w| w.back_on(c.boot_id.as_deref()));
                    if back {
                        self.back_from_reboot(ri, hi);
                    } else {
                        tracing::debug!(host = %c.host, "reconnected before rebooting; still waiting for the reboot");
                    }
                }
                HostPhase::Baking if self.bake_seconds(ri, hi) > 0 => {
                    // Resume the bake. Past its window, a host that was away
                    // reports afresh before it counts as baked, and a check
                    // still failing is followed through settle.
                    let until = self.bake_watch_until(ri, hi).unwrap_or(now);
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
                    if own && prev.is_some() && c.applied_revision == prev {
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
            let subject = self.live[ri].hosts[hi].host.clone();
            let pending = self.live[ri].record.runtime.phases.iter().find_map(|p| {
                p.gate
                    .as_ref()
                    .and_then(|g| g.pending.get(&subject).cloned())
            });
            if let Some(request_id) = pending {
                let tp = traceparent_of(&self.live[ri].span).unwrap_or_default();
                self.send_to(
                    ri,
                    hi,
                    server_message::Msg::Health(RunHealth {
                        target: String::new(),
                        revision: String::new(),
                        request_id,
                        scope: HealthScope::Rollout as i32,
                        modules: vec![],
                        until: None,
                        traceparent: tp,
                    }),
                );
            }
        }
    }

    fn on_disconnected(&mut self, host: &str) {
        let now = self.now();
        self.online.remove(host);
        for ri in 0..self.live.len() {
            let rows: Vec<usize> = (0..self.live[ri].hosts.len())
                .filter(|&hi| self.row_session(ri, hi).as_deref() == Some(host))
                .collect();
            for hi in rows {
                let subject = self.live[ri].hosts[hi].host.clone();
                let gate_pending = self.live[ri].record.runtime.phases.iter().any(|p| {
                    p.gate
                        .as_ref()
                        .is_some_and(|g| g.pending.contains_key(&subject))
                });
                let state = self.live[ri].hosts[hi].state;
                if awaits_reconnect(state) || gate_pending {
                    self.live[ri].hosts[hi].runtime.disconnected_at = Some(now);
                    self.touch_host(ri, hi);
                }
            }
        }
    }

    fn on_plan(&mut self, host: &str, plan: PlanResult) {
        let target = plan.target.clone();
        for ri in 0..self.live.len() {
            // An action rollout plans nothing: a plan (a drift check's) is
            // not its.
            if self.live[ri].record.runtime.action.is_some() {
                continue;
            }
            let Some(hi) = self.live[ri].find_target(host, &target) else {
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
            let mut stale_error = None;
            if stale && matches!(h.state, HostPhase::Applying | HostPhase::Baseline) {
                let approved = h.plan_digest.clone().unwrap_or_default();
                let refusals = h.runtime.stale_refusals + 1;
                let phase_index = h.runtime.phase_index;
                // The host never applied: its slot in the wave is given back,
                // as when it is deferred, so wave numbers count real waves.
                if let Some(pi) = phase_index
                    && let Some(p) = self.live[ri].record.runtime.phases.get_mut(pi)
                {
                    p.started = p.started.saturating_sub(1);
                    self.live[ri].dirty = true;
                }
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.stale_refusals = refusals;
                h.runtime.applied = false;
                h.runtime.request_id = None;
                h.runtime.apply_started_at = None;
                let message = format!(
                    "stale plan: the approved plan was {approved} but the host now plans {}",
                    plan.digest
                );
                if refusals >= MAX_STALE_REFUSALS {
                    // Re-planning again would only race the host once more:
                    // it waits for an operator to force apply, retry or skip.
                    let error = format!(
                        "{message}; the plan changed on {refusals} apply attempts \
                         and is not deterministic: force apply, retry or skip the host"
                    );
                    h.plan_digest = Some(plan.digest.clone());
                    h.plan = serde_json::from_slice(&plan.plan_json).ok();
                    h.runtime.approval_digest = non_empty(&plan.approval_digest);
                    h.runtime.error = Some(error.clone());
                    self.set_host_state(ri, hi, HostPhase::Held, "the plan is not deterministic");
                    continue;
                }
                h.state = HostPhase::PlanPending;
                stale_error = Some(message);
            } else if h.state == HostPhase::Applying && plan.error.is_empty() && !stale {
                // The agent is about to run a plan other than the stored one
                // (a force apply, or content that changed under an approved
                // intent): record what it really applies.
                let forced = h.runtime.force_plan;
                let from = h.plan_digest.clone().unwrap_or_default();
                let h = &mut self.live[ri].hosts[hi];
                h.plan_digest = Some(plan.digest.clone());
                h.plan = serde_json::from_slice(&plan.plan_json).ok();
                h.runtime.approval_digest = non_empty(&plan.approval_digest);
                h.runtime.plan_changes = Some(plan.changes);
                h.runtime.plan_removals = Some(plan.removals);
                h.runtime.destructive = plan.destructive;
                h.runtime.deferred_modules = plan.deferred_modules.clone();
                self.touch_host(ri, hi);
                if !forced && !from.is_empty() && from != plan.digest {
                    let host = host.to_owned();
                    self.command_audit(
                        "controller",
                        "rollout.host.plan_content_changed",
                        ri,
                        format!(
                            "{host}: applying {} under the approval of {}: the plan's content changed, its resources and actions did not",
                            short(&plan.digest),
                            short(&from)
                        ),
                        serde_json::json!({"host": host, "approved": from, "applied": plan.digest}),
                    );
                }
                continue;
            } else if !matches!(h.state, HostPhase::PlanPending | HostPhase::Planned) {
                continue;
            }
            if !plan.error.is_empty() && !stale && self.live[ri].hosts[hi].runtime.preview {
                // A cluster's preview may fail on what an earlier stage
                // provides: it waits for its stage to plan for real.
                let h = &mut self.live[ri].hosts[hi];
                if h.runtime.kind == TargetKind::Cluster {
                    h.runtime.error = Some(format!(
                        "the preview could not be planned: {}; plans again when its stage opens",
                        plan.error
                    ));
                    self.touch_host(ri, hi);
                    continue;
                }
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
                h.runtime.approval_digest = non_empty(&plan.approval_digest);
                h.runtime.plan_changes = Some(plan.changes);
                h.runtime.plan_removals = Some(plan.removals);
                h.runtime.destructive = plan.destructive;
                h.runtime.deferred_modules = plan.deferred_modules.clone();
                // A new plan: what failed before no longer describes it.
                h.runtime.failed_checks.clear();
                h.runtime.failed_module = None;
                // A valid plan with modules that could not be planned: shown
                // on the host; applying it fails those modules only. A plan
                // that replaced a stale one says so, so the host view shows
                // why it went back to planned.
                let mut notes: Vec<String> = stale_error.iter().cloned().collect();
                if !plan.module_errors.is_empty() {
                    notes.push(format!(
                        "modules that could not be planned: {}",
                        plan.module_errors.join("; ")
                    ));
                }
                h.runtime.error = (!notes.is_empty()).then(|| notes.join("; "));
            }
            self.record_drift(ri, hi);
            self.touch_host(ri, hi);
            let why = if stale_error.is_some() {
                "plan received; the approved plan was stale"
            } else {
                "plan received"
            };
            self.set_host_state(ri, hi, HostPhase::Planned, why);
        }
    }

    fn on_apply_result(&mut self, host: &str, result: ApplyResult) {
        let target = result.target.clone();
        for ri in 0..self.live.len() {
            if self.live[ri].record.runtime.action.is_some() {
                continue;
            }
            let Some(hi) = self.live[ri].find_target(host, &target) else {
                continue;
            };
            let state = self.live[ri].hosts[hi].state;
            let target = self.live[ri].hosts[hi].runtime.revision.clone();
            let prev = self.live[ri].record.runtime.previous_stable.clone();
            match state {
                HostPhase::Applying
                    if result.revision == target
                        && (result.plan_digest.is_empty()
                            || self.live[ri].hosts[hi].runtime.force_plan
                            || self.live[ri].hosts[hi].plan_digest.as_deref()
                                == Some(result.plan_digest.as_str())) =>
                {
                    self.live[ri].hosts[hi].result =
                        serde_json::to_value(ResultView::from(&result)).ok();
                    if !result.plan_digest.is_empty() {
                        self.live[ri].hosts[hi].plan_digest = Some(result.plan_digest.clone());
                    }
                    self.live[ri].hosts[hi].runtime.force_plan = false;
                    // A deferred module planned differently from the preview
                    // that was approved, so the agent left it alone. The host
                    // is re-planned for approval rather than failed.
                    if !result.succeeded && !result.stale_modules.is_empty() {
                        let why = format!(
                            "deferred {} {} planned differently from the approved preview",
                            if result.stale_modules.len() == 1 {
                                "module"
                            } else {
                                "modules"
                            },
                            result.stale_modules.join(", ")
                        );
                        let h = &mut self.live[ri].hosts[hi];
                        h.runtime.stale_refusals += 1;
                        h.runtime.request_id = None;
                        h.runtime.apply_started_at = None;
                        if h.runtime.stale_refusals >= MAX_STALE_REFUSALS {
                            h.runtime.error =
                                Some(format!("{why}; force apply, retry or skip the host"));
                            self.set_host_state(ri, hi, HostPhase::Held, &why);
                        } else {
                            h.runtime.error = Some(why.clone());
                            h.plan_digest = None;
                            self.set_host_state(ri, hi, HostPhase::PlanPending, &why);
                            self.request_plan(ri, hi);
                        }
                        continue;
                    }
                    // The agent accepted and ran the approved plan.
                    self.live[ri].hosts[hi].runtime.stale_refusals = 0;
                    if result.succeeded {
                        let changed = self.result_changes(ri, hi, &result);
                        self.live[ri].hosts[hi].runtime.bake =
                            Some(self.bake_window(ri, hi, changed));
                    }
                    if let Some(reboot) = &result.reboot {
                        self.await_reboot(ri, hi, reboot);
                    }
                    if result.succeeded {
                        if self.live[ri].hosts[hi].runtime.reboot.is_some() {
                            self.on_reboot_announced(ri, hi);
                        } else {
                            self.start_bake(ri, hi);
                        }
                    } else {
                        {
                            let h = &mut self.live[ri].hosts[hi];
                            h.runtime.failed_module = (!result.failed_module.is_empty())
                                .then(|| result.failed_module.clone());
                            h.runtime.failed_checks = result.failed_checks.clone();
                            h.runtime.error_detail = result_error_detail(&result);
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

    fn on_health(&mut self, agent: &str, report: HealthReport) {
        let scope = HealthScope::try_from(report.scope).unwrap_or(HealthScope::Unspecified);
        let now = self.now();
        let target = report.target.clone();
        // The row the report is about: the agent's host, or the cluster it
        // names as its executor.
        let host: &str = if target.is_empty() { agent } else { &target };
        tracing::debug!(buckle.host = %agent, target = %target, scope = ?scope, request = %report.request_id, revision = %report.revision, results = report.results.len(), "health report");
        for ri in 0..self.live.len() {
            let row = self.live[ri].find_target(agent, &target);
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
                let waived = row
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
                    if let Some(hi) = row {
                        self.live[ri].hosts[hi].health =
                            serde_json::to_value(HealthView::from(&report)).ok();
                        self.live[ri].hosts[hi].runtime.failed_checks = failures.clone();
                    }
                    self.fail_gate(ri, pi, host, &why);
                }
                continue;
            }
            let Some(hi) = row else {
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
            if self.live[ri].record.runtime.action.is_some() {
                continue;
            }
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
            .call(move |db| db.hosts().target(&h))
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
        // Registration precedes the Hello's labels in the store, so a late
        // host is admitted to a running rollout when it connects, not here.
        self.host_seen
            .insert(host.to_owned(), (record.status, record.effective_labels()));
    }

    async fn on_host_changed(&mut self, host: &str) {
        let h = host.to_owned();
        let record = self
            .store
            .call(move |db| db.hosts().target(&h))
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
        // A cluster target has no agent to push to: its spec is noted for
        // the executor pick, and an active one may join a running rollout
        // or the stragglers stage, as a host that connects does.
        if let Some(r) = &record
            && crate::targets::is_cluster(r)
        {
            if let Some(c) = &r.cluster {
                self.clusters.insert(host.to_owned(), c.spec.clone());
            }
            let seen = (r.status, r.effective_labels());
            if self.host_seen.get(host) == Some(&seen) {
                return;
            }
            self.host_seen.insert(host.to_owned(), seen);
            if r.status == HostStatus::Active {
                for ri in 0..self.live.len() {
                    self.admit_late_targets(ri, Some(host)).await;
                }
                self.maybe_add_straggler(host, r.applied_revision.as_deref())
                    .await;
            }
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
        {
            for ri in 0..self.live.len() {
                self.admit_late_targets(ri, Some(host)).await;
            }
            if self.connected(host) {
                self.on_connected(Connected {
                    host: host.to_owned(),
                    applied_revision: r.applied_revision.clone(),
                    last_plan_digest: r.last_plan_digest.clone(),
                    ..Default::default()
                })
                .await;
            }
        }
    }

    /// A forgotten host leaves every live rollout it has not finished:
    /// skipped (it no longer exists, so it is neither waited for nor counted
    /// as a failure) and dropped from pending gates.
    fn forget_host(&mut self, host: &str) {
        self.clusters.remove(host);
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
                && record.executor_of().is_none()
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

    /// Starts an operator's action on `target` (see [`Command::RunAction`]):
    /// refused while the target is applying in a rollout, paused, offline
    /// (a cluster: without a connected executor) or running another action.
    async fn run_action(
        &mut self,
        actor: &str,
        target: String,
        module: String,
        action: String,
        revision: String,
        note: String,
    ) -> Result<String, CommandError> {
        let applying = self.live.iter().any(|l| {
            l.hosts.iter().any(|h| {
                h.host == target && (h.state.in_flight() || h.state == HostPhase::RollingBack)
            })
        });
        if applying {
            return Err(CommandError::conflict(
                format!("{target} is applying in a rollout."),
                &["Run the action once the rollout has finished with it."],
            ));
        }
        let t = target.clone();
        let (record, running) = self
            .store
            .call(move |db| Ok((db.hosts().target(&t)?, db.runs().running_action(&t)?)))
            .await
            .map_err(CommandError::internal)?;
        let record =
            record.ok_or_else(|| CommandError::not_found(format!("No target '{target}'.")))?;
        if record.status != HostStatus::Active {
            return Err(CommandError::conflict(
                format!(
                    "{target} is {:?}; actions run on active targets only.",
                    record.status
                ),
                &["Resume (or approve) it first."],
            ));
        }
        if let Some(running) = running {
            return Err(CommandError::conflict(
                format!("{running} is still running on {target}."),
                &["Wait for it to finish; its run is on the target's page."],
            ));
        }
        let session = match record.kind {
            TargetKind::Host if self.eligible(&target) => target.clone(),
            TargetKind::Host => {
                return Err(CommandError::conflict(
                    format!("{target} is offline."),
                    &["Run the action once its agent reconnects."],
                ));
            }
            TargetKind::Cluster => self
                .executors
                .get(&target)
                .filter(|e| {
                    self.eligible(e)
                        && self
                            .capabilities
                            .get(*e)
                            .is_some_and(|c| c.iter().any(|c| c == TARGETS_CAPABILITY))
                })
                .cloned()
                .ok_or_else(|| {
                    CommandError::conflict(
                        format!("No executor of {target} is connected."),
                        &["Run the action once a rollout has picked an executor that is online."],
                    )
                })?,
        };
        let now = self.now();
        let run_id = crate::store::RunRecord::new_id();
        let label = format!("{module}/{action}");
        let span = tracing::info_span!("action.run", buckle.host = %target, buckle.module = %module, buckle.action = %action, buckle.run = %run_id);
        let run = crate::store::RunRecord {
            id: run_id.clone(),
            host: target.clone(),
            rollout: None,
            revision: revision.clone(),
            plan_digest: String::new(),
            started_at: now,
            finished_at: now,
            outcome: crate::store::RUNNING.to_owned(),
            changes: 0,
            removals: 0,
            error: None,
            trace_id: trace_id_of(&span),
            outcome_json: None,
            action: Some(label.clone()),
        };
        self.store
            .call(move |db| db.runs().add(&run).map(|_| ()))
            .await
            .map_err(CommandError::internal)?;
        let m = msg(server_message::Msg::Action(pb::RunAction {
            action_id: run_id.clone(),
            module,
            action,
            revision,
            traceparent: traceparent_of(&span).unwrap_or_default(),
            target: String::new(),
        }));
        self.outbox.push(match record.kind {
            TargetKind::Host => Out::Msg(session, Box::new(m)),
            TargetKind::Cluster => Out::Targeted {
                session,
                target: target.clone(),
                msg: Box::new(stamp_target(m, &target)),
            },
        });
        let noun = match record.kind {
            TargetKind::Host => "host",
            TargetKind::Cluster => "target",
        };
        let message = if note.is_empty() {
            format!("{actor} ran {label} on {target}")
        } else {
            format!("{actor} ran {label} on {target}: {note}")
        };
        self.audit(
            actor,
            &format!("{noun}.action.run"),
            &target,
            message,
            serde_json::json!({"module": label.split('/').next(), "action": label, "note": note, "run": run_id}),
            trace_id_of(&span),
        );
        Ok(run_id)
    }

    /// Starts an action rollout (see [`Command::StartAction`]): refused
    /// while another rollout is under way or queued, and without a stable
    /// revision (whose phases it follows).
    async fn start_action(
        &mut self,
        actor: &str,
        action: RolloutAction,
        targets: BTreeMap<String, String>,
        note: &str,
    ) -> Result<String, CommandError> {
        if let Some(l) = self
            .live
            .iter()
            .find(|l| l.record.is_active() || l.record.state == RolloutState::Detected)
        {
            return Err(CommandError::conflict(
                format!(
                    "Rollout {} is under way.",
                    buckle_api::rollout_name(&l.record.id).unwrap_or_else(|| l.record.id.clone())
                ),
                &["Run the action once it has finished, or abort it."],
            ));
        }
        if targets.is_empty() {
            return Err(CommandError::invalid(
                format!("No target offers {}.", action.label),
                &["Check the selector, and that the targets run a revision declaring the action."],
            ));
        }
        let stable = self.stable.clone().ok_or_else(|| {
            CommandError::conflict(
                "There is no stable revision yet: an action rollout follows its phases.",
                &["Roll out a revision first."],
            )
        })?;
        let rev = self
            .revisions
            .load(&stable)
            .await
            .map_err(CommandError::internal)?;
        let now = self.now();
        let id = self.new_rollout_id().await?;
        let n = targets.len();
        let record = RolloutRecord {
            id: id.clone(),
            revision: stable.clone(),
            state: RolloutState::Detected,
            policy: rev.fleet.rollout.clone(),
            started_at: now,
            finished_at: None,
            triggered_by: actor.to_owned(),
            runtime: RolloutRuntime {
                note: note.to_owned(),
                revision_message: format!("{} ({})", action.label, action.key()),
                action: Some(action.clone()),
                ..Default::default()
            },
        };
        let span = run_span(&id, &stable, None);
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
        let message = if note.is_empty() {
            format!(
                "rollout of {} on {n} targets created by {actor}",
                action.key()
            )
        } else {
            format!(
                "rollout of {} on {n} targets created by {actor}: {note}",
                action.key()
            )
        };
        self.command_audit(
            actor,
            "rollout.detected",
            ri,
            message,
            serde_json::json!({
                "module": action.module,
                "action": action.key(),
                "selector": action.selector,
                "targets": targets.keys().collect::<Vec<_>>(),
                "note": note,
            }),
        );
        self.freeze(ri, Some(targets))
            .await
            .map_err(CommandError::internal)?;
        Ok(id)
    }

    /// An action rollout's row is ready as soon as it is frozen (or
    /// re-planned): its "plan" is the action, one digest for every target.
    fn action_planned(&mut self, ri: usize, hi: usize) {
        let Some(action) = self.live[ri].record.runtime.action.clone() else {
            return;
        };
        if !matches!(
            self.live[ri].hosts[hi].state,
            HostPhase::PlanPending | HostPhase::Planned
        ) {
            return;
        }
        {
            let h = &mut self.live[ri].hosts[hi];
            h.plan_digest = Some(action.digest());
            h.plan = None;
            h.runtime.plan_changes = Some(1);
            h.runtime.plan_removals = Some(0);
            h.runtime.destructive = action.confirm;
            h.runtime.preview = false;
        }
        self.touch_host(ri, hi);
        self.set_host_state(ri, hi, HostPhase::Planned, "ready to run the action");
    }

    /// Sends row `hi` its action, its run stored (running) with the
    /// checkpoint before the message goes.
    fn send_action(&mut self, ri: usize, hi: usize, action: &RolloutAction) {
        let now = self.now();
        let span = self.host_span(ri, hi);
        let run_id = crate::store::RunRecord::new_id();
        let (target, revision, rollout) = {
            let live = &self.live[ri];
            let h = &live.hosts[hi];
            (
                h.host.clone(),
                h.runtime.revision.clone(),
                live.record.id.clone(),
            )
        };
        self.new_runs.push(crate::store::RunRecord {
            id: run_id.clone(),
            host: target,
            rollout: Some(rollout),
            revision: revision.clone(),
            plan_digest: action.digest(),
            started_at: now,
            finished_at: now,
            outcome: crate::store::RUNNING.to_owned(),
            changes: 0,
            removals: 0,
            error: None,
            trace_id: trace_id_of(&span),
            outcome_json: None,
            action: Some(action.key()),
        });
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.applied = true;
            h.runtime.apply_started_at = Some(now);
            h.runtime.apply_resends = 0;
            h.runtime.reboot = None;
            h.runtime.bake = None;
            h.runtime.action_run = Some(run_id.clone());
        }
        self.set_host_state(ri, hi, HostPhase::Applying, "action sent");
        self.touch_host(ri, hi);
        self.send_to(
            ri,
            hi,
            server_message::Msg::Action(pb::RunAction {
                action_id: run_id,
                module: action.module.clone(),
                action: action.action.clone(),
                revision,
                traceparent: traceparent_of(&span).unwrap_or_default(),
                target: String::new(),
            }),
        );
    }

    /// The result of an action an action rollout sent: on success the row
    /// bakes (after its reboot, when the action asked for one), else it
    /// fails against its phase's budget.
    fn on_action_result(&mut self, host: &str, a: pb::ActionResult) {
        for ri in 0..self.live.len() {
            if self.live[ri].record.runtime.action.is_none() {
                continue;
            }
            let Some(hi) = self.live[ri].find_target(host, &a.target) else {
                continue;
            };
            let h = &self.live[ri].hosts[hi];
            if h.state != HostPhase::Applying
                || h.runtime.action_run.as_deref() != Some(a.action_id.as_str())
            {
                continue;
            }
            let outcome: Option<serde_json::Value> = serde_json::from_slice(&a.outcome_json).ok();
            self.live[ri].hosts[hi].result = serde_json::to_value(ResultView {
                revision: a.revision.clone(),
                plan_digest: a.plan_digest.clone(),
                succeeded: a.succeeded,
                changes: a.changes,
                removals: 0,
                applied_revision: String::new(),
                failed_module: if a.succeeded {
                    String::new()
                } else {
                    a.module.clone()
                },
                rolled_back_modules: Vec::new(),
                not_started_modules: Vec::new(),
                error: a.error.clone(),
                failed_checks: Vec::new(),
                outcome,
            })
            .ok();
            if a.succeeded {
                self.live[ri].hosts[hi].runtime.bake = Some(self.bake_window(ri, hi, None));
                if let Some(reboot) = &a.reboot {
                    self.await_reboot(ri, hi, reboot);
                }
                if self.live[ri].hosts[hi].runtime.reboot.is_some() {
                    self.on_reboot_announced(ri, hi);
                } else {
                    self.start_bake(ri, hi);
                }
                continue;
            }
            let why = if a.error.is_empty() {
                "the action failed".to_owned()
            } else {
                a.error.clone()
            };
            {
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.failed_module = Some(a.module.clone());
                h.runtime.error_detail = a.error_detail.as_ref().map(error_view);
                h.runtime.error = Some(why.clone());
            }
            self.fail_host(ri, hi, &why);
        }
    }

    async fn on_command(&mut self, actor: &str, command: Command) -> Result<String, CommandError> {
        match command {
            Command::RunAction {
                target,
                module,
                action,
                revision,
                note,
            } => {
                self.run_action(actor, target, module, action, revision, note)
                    .await
            }
            Command::Start { revision, note } => self.start(&revision, actor, &note).await,
            Command::StartAction {
                action,
                targets,
                note,
            } => self.start_action(actor, action, targets, &note).await,
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
                if self.live[ri].record.runtime.action.is_some() {
                    return Err(CommandError::conflict(
                        "An action rollout cannot be rolled back.",
                        &["Pause or abort it; run another action to undo what it did."],
                    ));
                }
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
                    HostPhase::Failed
                        | HostPhase::Held
                        | HostPhase::Skipped
                        | HostPhase::Missed
                        | HostPhase::PlanPending
                        | HostPhase::Planned
                ) {
                    return Err(CommandError::conflict(
                        format!(
                            "Host '{host}' is past its plan (applying, baking or done), so it cannot be re-planned now."
                        ),
                        &["Wait for its apply and bake to finish; a failed host can be retried."],
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
            Command::RetryFailed { rollout, note } => {
                let ri = self.find_rollout(&rollout)?;
                let record = &self.live[ri].record;
                if (record.is_terminal() && !record.runtime.stragglers_open)
                    || record.state == RolloutState::RollingBack
                {
                    return Err(CommandError::conflict(
                        "This rollout cannot retry its hosts in its current state.",
                        &["Only a running or paused rollout retries its failed hosts."],
                    ));
                }
                let failed: Vec<usize> = (0..self.live[ri].hosts.len())
                    .filter(|&i| self.live[ri].hosts[i].state == HostPhase::Failed)
                    .collect();
                if failed.is_empty() {
                    return Err(CommandError::conflict(
                        "No host in this rollout has failed, so there is nothing to retry.",
                        &[],
                    ));
                }
                let names: Vec<String> = failed
                    .iter()
                    .map(|&i| self.live[ri].hosts[i].host.clone())
                    .collect();
                self.command_audit(
                    actor,
                    "rollout.retry",
                    ri,
                    format!(
                        "{actor} retried {} failed host(s): {}. {note}",
                        names.len(),
                        names.join(", ")
                    ),
                    serde_json::json!({"hosts": names, "note": note}),
                );
                for hi in failed {
                    self.replan(ri, hi, actor);
                }
                Ok(rollout)
            }
            Command::SkipBake {
                rollout,
                host,
                note,
            } => self.skip_bake(actor, rollout, host, note),
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
                let state = self.live[ri].hosts[hi].state;
                if !matches!(
                    state,
                    HostPhase::Held
                        | HostPhase::Planned
                        | HostPhase::Failed
                        | HostPhase::Healthy
                        | HostPhase::Skipped
                        | HostPhase::Missed
                ) {
                    return Err(CommandError::conflict(
                        format!(
                            "Host '{host}' is {}, so it cannot be force applied now.",
                            state.as_str()
                        ),
                        &["Wait for the host's plan or apply to finish, then force it."],
                    ));
                }
                let Some(session) = self.row_session(ri, hi).filter(|s| self.connected(s)) else {
                    let what = match self.live[ri].hosts[hi].runtime.kind {
                        TargetKind::Host => format!("Host '{host}' is not connected."),
                        TargetKind::Cluster => {
                            format!("Cluster '{host}' has no connected executor.")
                        }
                    };
                    return Err(CommandError::conflict(
                        what,
                        &["Wait for the agent to reconnect, then force it again."],
                    ));
                };
                // Applying whatever the host plans needs an agent that can
                // skip its digest check; an older one still applies a host
                // held at baseline with its approved plan.
                let force_plan = self
                    .capabilities
                    .get(&session)
                    .is_some_and(|c| c.iter().any(|c| c == "force_plan"));
                if !force_plan && state != HostPhase::Held {
                    return Err(CommandError::conflict(
                        format!("Host '{host}' runs an agent that cannot force apply a plan."),
                        &["Upgrade the host's Buckle agent, or retry the host to re-plan it."],
                    ));
                }
                let released = self.live[ri].hosts[hi].runtime.released;
                self.live[ri].hosts[hi].runtime.released = true;
                if !self.may_apply(ri, hi) {
                    self.live[ri].hosts[hi].runtime.released = released;
                    return Err(CommandError::conflict(
                        format!("Rollout '{rollout}' is not running, so nothing is applied."),
                        &["Resume the rollout first, then force the host."],
                    ));
                }
                let what = match state {
                    HostPhase::Held if self.live[ri].hosts[hi].runtime.held_at_baseline => {
                        "past its failing baseline"
                    }
                    HostPhase::Planned => "without approval",
                    HostPhase::Healthy => "again",
                    _ => "with whatever it plans now",
                };
                self.command_audit(
                    actor,
                    "rollout.host.force",
                    ri,
                    format!("{actor} force applied {host} {what}. {note}"),
                    serde_json::json!({"host": host, "note": note, "from": state.as_str(), "force_plan": force_plan}),
                );
                {
                    let h = &mut self.live[ri].hosts[hi];
                    h.runtime.force = true;
                    h.runtime.force_plan = force_plan;
                    h.runtime.failed_checks.clear();
                    h.runtime.failed_module = None;
                    h.runtime.error = None;
                    h.runtime.error_detail = None;
                    h.runtime.needs_attention = false;
                    h.runtime.failing_since.clear();
                    h.runtime.request_id = None;
                    h.runtime.bake_until = None;
                    h.runtime.bake = None;
                    h.runtime.stale_refusals = 0;
                    if state != HostPhase::Held {
                        h.runtime.attempts += 1;
                    }
                }
                self.send_apply(ri, hi);
                self.unpause_if_cleared(ri, actor);
                Ok(rollout)
            }
        }
    }

    /// Ends the bake of `host` (or of every baking row) now, as if its
    /// window had passed: a row whose checks are failing keeps baking, so
    /// that skipping a bake never passes a regression.
    fn skip_bake(
        &mut self,
        actor: &str,
        rollout: String,
        host: Option<String>,
        note: String,
    ) -> Result<String, CommandError> {
        let ri = self.find_rollout(&rollout)?;
        let record = &self.live[ri].record;
        if (record.is_terminal() && !record.runtime.stragglers_open)
            || record.state == RolloutState::RollingBack
        {
            return Err(CommandError::conflict(
                "This rollout cannot skip a bake in its current state.",
                &[],
            ));
        }
        let rows: Vec<usize> = match &host {
            Some(name) => {
                let hi = self.find_host(ri, name)?;
                if self.live[ri].hosts[hi].state != HostPhase::Baking {
                    return Err(CommandError::conflict(
                        format!("Host '{name}' is not baking, so there is no bake to skip."),
                        &[],
                    ));
                }
                vec![hi]
            }
            None => (0..self.live[ri].hosts.len())
                .filter(|&i| self.live[ri].hosts[i].state == HostPhase::Baking)
                .collect(),
        };
        if rows.is_empty() {
            return Err(CommandError::conflict(
                "No host in this rollout is baking, so there is no bake to skip.",
                &[],
            ));
        }
        let (failing, clear): (Vec<usize>, Vec<usize>) = rows
            .into_iter()
            .partition(|&i| !self.live[ri].hosts[i].runtime.failing_since.is_empty());
        let names = |rows: &[usize]| -> Vec<String> {
            rows.iter()
                .map(|&i| self.live[ri].hosts[i].host.clone())
                .collect()
        };
        let (skipped, kept) = (names(&clear), names(&failing));
        if skipped.is_empty() {
            return Err(CommandError::conflict(
                format!(
                    "{} {} failing checks, so the bake cannot be skipped.",
                    kept.join(", "),
                    if kept.len() == 1 { "has" } else { "have" }
                ),
                &["Wait for the checks to pass, or retry or skip the host."],
            ));
        }
        let (action, subject) = match &host {
            Some(h) => ("rollout.host.skip_bake", h.clone()),
            None => (
                "rollout.skip_bake",
                format!("{} host(s): {}", skipped.len(), skipped.join(", ")),
            ),
        };
        let kept_note = if kept.is_empty() {
            String::new()
        } else {
            format!(" ({} still baking: failing checks)", kept.join(", "))
        };
        self.command_audit(
            actor,
            action,
            ri,
            format!("{actor} skipped the bake of {subject}{kept_note}. {note}"),
            serde_json::json!({"hosts": skipped, "failing": kept, "note": note}),
        );
        for hi in clear {
            self.finish_step(ri, hi);
        }
        Ok(rollout)
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
        // An approval gate keeps a change from rolling out blind: a phase is
        // approved only once every phase before it has finished, so its
        // approver has seen how they went.
        let running: Vec<&str> = self.live[ri].record.runtime.phases[..pi]
            .iter()
            .filter(|p| !matches!(p.state, PhaseState::Done | PhaseState::Skipped))
            .map(|p| p.name.as_str())
            .collect();
        if !running.is_empty() {
            return Err(CommandError::conflict(
                format!(
                    "Phase '{phase}' can be approved once {} {} finished.",
                    running.join(", "),
                    if running.len() == 1 { "has" } else { "have" }
                ),
                &["Review its plans now; approve it when the rollout reaches it."],
            ));
        }
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
        // The intents of the approved plans, so that a host whose re-plan
        // only changes content stays covered.
        let mut intents: Vec<String> = hosts
            .iter()
            .filter_map(|&i| {
                let h = &self.live[ri].hosts[i];
                let covered = h.plan_digest.as_ref().is_some_and(|d| digests.contains(d));
                covered.then(|| h.runtime.approval_digest.clone()).flatten()
            })
            .collect();
        intents.sort();
        intents.dedup();
        let now = self.now();
        let id = self.live[ri].record.id.clone();
        let (p2, a2, d2, i2, n2) = (
            phase.to_owned(),
            actor.to_owned(),
            digests.clone(),
            intents.clone(),
            note.to_owned(),
        );
        let id2 = id.clone();
        let approval_id = self
            .store
            .call(move |db| db.approvals().add(&id2, &p2, &a2, &d2, &i2, &n2, now))
            .await
            .map_err(CommandError::internal)?;
        self.live[ri].approvals.push(ApprovalRecord {
            id: approval_id,
            rollout: id.clone(),
            phase: phase.to_owned(),
            approver: actor.to_owned(),
            plan_digests: digests.clone(),
            approval_digests: intents,
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

    /// A fresh rollout id whose name no earlier rollout has, so that a name
    /// keeps finding one rollout (a clash is 1 in 2^33 per pair; the retries
    /// only bound the loop).
    async fn new_rollout_id(&self) -> Result<String, CommandError> {
        for _ in 0..8 {
            let id = new_id();
            let Some(name) = buckle_api::RolloutName::of(&id) else {
                return Ok(id);
            };
            let taken = self
                .store
                .call(move |db| db.rollouts().ids_named(&name))
                .await
                .map_err(CommandError::internal)?;
            if taken.is_empty() {
                return Ok(id);
            }
        }
        Ok(new_id())
    }

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
        let id = self.new_rollout_id().await?;
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
                revision_message: record.message.clone(),
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
                SupersedePolicy::Latest => {
                    self.skip_queued(0..ri, &id);
                    return Ok(id);
                }
                SupersedePolicy::Sequential => return Ok(id),
                SupersedePolicy::Replace => {
                    self.skip_queued(0..ri, &id);
                    for rj in 0..ri {
                        if self.live[rj].record.is_active() {
                            self.live[rj].record.runtime.superseded_by = Some(id.clone());
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
        self.freeze(ri, None)
            .await
            .map_err(CommandError::internal)?;
        Ok(id)
    }

    /// Every target (host or cluster) with its latest facts, as the
    /// selectors see them. With `sync`, the clusters that revision declares
    /// are materialised as rows first. Refreshes the cluster specs the
    /// executor pick and the stage layout read.
    async fn load_targets(
        &mut self,
        sync: Option<Arc<Revision>>,
    ) -> buckle_core::Result<Vec<(crate::store::HostRecord, serde_json::Value)>> {
        let now = self.now();
        let targets = self
            .store
            .call(move |db| {
                if let Some(rev) = &sync {
                    crate::targets::sync_declared(db, rev, now)?;
                }
                let mut out = Vec::new();
                for h in db.hosts().targets()? {
                    let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                    out.push((h, facts.unwrap_or(serde_json::Value::Null)));
                }
                Ok(out)
            })
            .await?;
        self.clusters = targets
            .iter()
            .filter_map(|(h, _)| h.cluster.as_ref().map(|c| (h.id.clone(), c.spec.clone())))
            .collect();
        Ok(targets)
    }

    /// Freezes the target set and requests plans everywhere. An action
    /// rollout freezes `only` (target → the revision it runs the action
    /// from), each laid out as one step, and plans nothing.
    async fn freeze(
        &mut self,
        ri: usize,
        only: Option<BTreeMap<String, String>>,
    ) -> buckle_core::Result<()> {
        let revision = self.live[ri].revision.clone();
        let hosts = self.load_targets(Some(revision)).await?;
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
                stages: 1,
                open_stage: 0,
            })
            .collect();
        // An action rollout has nothing to roll back to.
        live.record.runtime.previous_stable = match &only {
            Some(_) => None,
            None => self.stable.clone(),
        };
        live.record.started_at = now;
        let mut untargeted = Vec::new();
        for (host, facts) in &hosts {
            self.host_seen
                .insert(host.id.clone(), (host.status, host.effective_labels()));
            // An in-cluster executor runs no modules of its own.
            if host.status != HostStatus::Active || host.executor_of().is_some() {
                continue;
            }
            let revision = match &only {
                Some(only) => match only.get(&host.id) {
                    Some(r) => r.clone(),
                    None => continue,
                },
                None => live.record.revision.clone(),
            };
            let ctx = crate::fleet::record_context(host, facts, fleet);
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
                        revision,
                        phase_index: Some(pi),
                        kind: host.kind,
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
        // An action runs on each target as one step: no dependency stages.
        if only.is_none() {
            for pi in 0..phases.len() {
                self.compute_stages(ri, pi, &hosts);
            }
        }
        self.set_rollout_state(ri, RolloutState::Planning, "controller", &count);
        for hi in 0..n {
            self.request_plan(ri, hi);
        }
        if only.is_none() {
            self.preview_clusters(ri).await;
        }
        Ok(())
    }

    /// Plans the clusters whose stage has not opened yet as previews, through
    /// an executor free now. One without a free executor stays unplanned
    /// until its stage opens, as does one whose preview fails.
    async fn preview_clusters(&mut self, ri: usize) {
        let rows: Vec<usize> = (0..self.live[ri].hosts.len())
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.runtime.kind == TargetKind::Cluster
                    && h.state == HostPhase::PlanPending
                    && !self.stage_open(ri, hi)
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        self.assign_executors(ri, &rows, false).await;
        for hi in rows {
            if self.live[ri].hosts[hi].runtime.executor.is_some() {
                self.request_preview(ri, hi);
            }
        }
    }

    /// Plans again the rows of phase `p` whose stage is open but whose plan
    /// is a preview (made before it opened): the fresh plan is what applies,
    /// and needs approval again only where the approval does not cover it
    /// (see [`Controller::on_plan`] for the drift it records).
    fn refresh_previews(&mut self, ri: usize, p: usize) -> bool {
        let rows: Vec<usize> = self.live[ri]
            .phase_hosts(p)
            .into_iter()
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.runtime.preview
                    && matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
                    && self.stage_open(ri, hi)
            })
            .collect();
        for &hi in &rows {
            {
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.preview = false;
                if let Some(binding) = h
                    .runtime
                    .approval_digest
                    .clone()
                    .or_else(|| h.plan_digest.clone())
                {
                    h.runtime.previewed = Some(binding);
                }
                h.plan_digest = None;
                h.runtime.approval_digest = None;
            }
            self.set_host_state(
                ri,
                hi,
                HostPhase::PlanPending,
                "its stage opened; planning again",
            );
            self.request_plan(ri, hi);
        }
        !rows.is_empty()
    }

    /// Lays phase `pi`'s targets out in dependency stages (see
    /// [`stages`]) from the cross-layer `needs` of the modules selected on
    /// its hosts and of the workloads on its clusters, and records each
    /// target's steps. Hosts' first steps keep any plan they have unless
    /// the step narrowed to a module subset, in which case they re-plan.
    fn compute_stages(
        &mut self,
        ri: usize,
        pi: usize,
        targets: &[(crate::store::HostRecord, serde_json::Value)],
    ) {
        let rows = self.live[ri].phase_hosts(pi);
        let (hosts, clusters) = {
            let live = &self.live[ri];
            let revision = &live.revision;
            let mut hosts = Vec::new();
            let mut clusters = Vec::new();
            for &hi in &rows {
                let row = &live.hosts[hi];
                match row.runtime.kind {
                    TargetKind::Host => {
                        let ctx = targets
                            .iter()
                            .find(|(h, _)| h.id == row.host)
                            .map(|(h, f)| (h, f));
                        let modules = crate::views::selected_modules(revision, None, ctx)
                            .into_iter()
                            .map(|name| stages::ModuleUnit {
                                needs: revision
                                    .module(&name)
                                    .map(|m| {
                                        m.config
                                            .needs()
                                            .into_iter()
                                            .map(stages::Need::from)
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                name,
                            })
                            .collect();
                        hosts.push(stages::HostInput {
                            id: row.host.clone(),
                            modules,
                        });
                    }
                    TargetKind::Cluster => {
                        if let Some(c) = self.cluster_input(ri, hi, &rows, targets) {
                            clusters.push(c);
                        }
                    }
                }
            }
            (hosts, clusters)
        };
        let staged = stages::layer(&hosts, &clusters);
        let mut replan = Vec::new();
        for &hi in &rows {
            let steps = staged
                .steps
                .get(&self.live[ri].hosts[hi].host)
                .cloned()
                .unwrap_or_else(|| vec![stages::whole()]);
            let h = &mut self.live[ri].hosts[hi];
            if h.runtime.steps == steps
                || (h.runtime.steps.is_empty() && steps == [stages::whole()])
            {
                continue;
            }
            let was = h.runtime.current_step();
            h.runtime.set_steps(steps);
            let now_step = h.runtime.current_step();
            if was.select() != now_step.select() && h.plan_digest.is_some() {
                replan.push(hi);
            }
            self.live[ri].dirty_hosts.insert(hi);
        }
        let phase = &mut self.live[ri].record.runtime.phases[pi];
        if phase.stages != staged.stages {
            phase.stages = staged.stages;
            self.live[ri].dirty = true;
        }
        for hi in replan {
            self.live[ri].hosts[hi].plan_digest = None;
            self.set_host_state(
                ri,
                hi,
                HostPhase::PlanPending,
                "its step changed; re-planning",
            );
            self.request_plan(ri, hi);
        }
    }

    /// The stage input for cluster row `hi`: its node hosts among `rows` and
    /// the workloads selected on it.
    fn cluster_input(
        &self,
        ri: usize,
        hi: usize,
        rows: &[usize],
        targets: &[(crate::store::HostRecord, serde_json::Value)],
    ) -> Option<stages::ClusterInput> {
        let live = &self.live[ri];
        let cluster = &live.hosts[hi].host;
        let spec = self.clusters.get(cluster)?;
        let fleet = &live.revision.fleet;
        let record = |id: &str| targets.iter().find(|(h, _)| h.id == id);
        let nodes_filter = spec
            .nodes
            .as_deref()
            .and_then(|raw| buckle_core::Filter::new(raw).ok());
        let nodes = rows
            .iter()
            .map(|&i| &live.hosts[i])
            .filter(|r| r.runtime.kind == TargetKind::Host)
            .filter(|r| {
                nodes_filter.as_ref().is_some_and(|f| {
                    record(&r.host).is_some_and(|(h, facts)| {
                        crate::fleet::record_context(h, facts, fleet).matches(f)
                    })
                })
            })
            .map(|r| r.host.clone())
            .collect();
        let (rec, facts) = record(cluster)?;
        let ctx = crate::fleet::record_context(rec, facts, fleet);
        let workloads = buckle_core::engine::selected_workloads(&live.revision, &ctx)
            .into_iter()
            .map(|w| stages::WorkloadUnit {
                name: w.config.name.clone(),
                needs: w
                    .config
                    .needs()
                    .into_iter()
                    .map(stages::Need::from)
                    .collect(),
            })
            .collect();
        Some(stages::ClusterInput {
            id: cluster.clone(),
            nodes,
            workloads,
        })
    }

    /// Compares row `hi`'s fresh plan with the preview it replaced, if any:
    /// a changed plan is recorded (audit `rollout.target.plan_drifted`) and
    /// shown on the row. Whether it needs approval again follows from the
    /// phase's policy and what its approvals cover, as for any plan.
    fn record_drift(&mut self, ri: usize, hi: usize) {
        let (target, from, to, phase) = {
            let h = &mut self.live[ri].hosts[hi];
            let Some(from) = h.runtime.previewed.take() else {
                return;
            };
            let to = h
                .runtime
                .approval_digest
                .clone()
                .or_else(|| h.plan_digest.clone())
                .unwrap_or_default();
            if from == to {
                h.runtime.drifted_from = None;
                return;
            }
            h.runtime.drifted_from = Some(from.clone());
            (h.host.clone(), from, to, h.phase.clone())
        };
        self.command_audit(
            "controller",
            "rollout.target.plan_drifted",
            ri,
            format!(
                "{target}: the plan changed since its preview ({} → {})",
                short(&from),
                short(&to)
            ),
            serde_json::json!({"host": target, "phase": phase, "previewed": from, "planned": to}),
        );
    }

    /// Asks for row `hi`'s plan: the whole revision, or the modules of its
    /// current step. A cluster's plan, and any later step's, waits for its
    /// stage to open (it depends on what the stages before it do), so the
    /// request is a no-op until then and is made when the stage opens.
    fn request_plan(&mut self, ri: usize, hi: usize) {
        self.send_plan_request(ri, hi, false);
    }

    /// Asks for a cluster row's plan before its stage opens: a preview,
    /// planned again when the stage opens (see
    /// [`Controller::refresh_previews`]).
    fn request_preview(&mut self, ri: usize, hi: usize) {
        self.send_plan_request(ri, hi, true);
    }

    /// A plan requested before the row's stage opens is a preview.
    fn send_plan_request(&mut self, ri: usize, hi: usize, early: bool) {
        if self.live[ri].record.runtime.action.is_some() {
            self.action_planned(ri, hi);
            return;
        }
        let open = self.stage_open(ri, hi);
        if self.live[ri].hosts[hi].runtime.plan_deferred() && !open && !early {
            return;
        }
        if !self.row_eligible(ri, hi) {
            return;
        }
        let Some(session) = self.row_session(ri, hi) else {
            return;
        };
        if self.live[ri].hosts[hi].runtime.preview != !open {
            self.live[ri].hosts[hi].runtime.preview = !open;
            self.live[ri].dirty_hosts.insert(hi);
        }
        let target = self.row_target(ri, hi);
        let revision = self.live[ri].hosts[hi].runtime.revision.clone();
        let stable = self.stable.as_deref() == Some(revision.as_str());
        self.outbox.push(Out::AssignAndPlan {
            session,
            target,
            revision,
            stable,
        });
    }

    /// The task selector of row `hi`'s current step (empty: the whole target).
    fn row_select(&self, ri: usize, hi: usize) -> String {
        self.live[ri].hosts[hi].runtime.current_step().select()
    }

    /// Plans row `hi` again. A row that failed, was held, skipped or missed
    /// is also released to apply (a retry); one still waiting to start (to
    /// plan, planned) only gets a fresh plan and keeps its place.
    fn replan(&mut self, ri: usize, hi: usize, actor: &str) {
        {
            let h = &mut self.live[ri].hosts[hi];
            let retry = !matches!(h.state, HostPhase::PlanPending | HostPhase::Planned);
            h.runtime.failed_checks.clear();
            h.runtime.failed_module = None;
            h.runtime.error = None;
            h.runtime.error_detail = None;
            h.runtime.needs_attention = false;
            h.runtime.failing_since.clear();
            h.runtime.request_id = None;
            h.runtime.bake_until = None;
            h.runtime.bake = None;
            if retry {
                h.runtime.attempts += 1;
                h.runtime.released = true;
            }
            h.runtime.stale_refusals = 0;
            h.runtime.force_plan = false;
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
        self.send_to(
            ri,
            hi,
            server_message::Msg::Health(RunHealth {
                target: String::new(),
                revision: String::new(),
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
            target: String::new(),
            revision: h.runtime.revision.clone(),
            plan_digest: digest.clone(),
            approval_token: approval_token(&self.key, &rollout, &phase, &digest),
            waived_checks: h.runtime.waived_checks.clone(),
            rollout_id: rollout,
            phase,
            force: h.runtime.force,
            traceparent: traceparent_of(&span).unwrap_or_default(),
            select: self.row_select(ri, hi),
            force_plan: h.runtime.force_plan,
            approval_digest: h.runtime.approval_digest.clone().unwrap_or_default(),
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
        let any_digest = h.runtime.force_plan || h.runtime.approval_digest.is_some();
        runs.into_iter().find(|r| {
            (any_digest || r.plan_digest == digest)
                && r.revision == revision
                && r.finished_at >= since
        })
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
        self.send_to(ri, hi, server_message::Msg::Apply(apply));
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
        if let Some(action) = self.live[ri].record.runtime.action.clone() {
            self.send_action(ri, hi, &action);
            return;
        }
        let now = self.now();
        let apply = self.build_apply(ri, hi);
        let h = &mut self.live[ri].hosts[hi];
        h.runtime.applied = true;
        h.runtime.apply_started_at = Some(now);
        h.runtime.apply_resends = 0;
        h.runtime.reboot = None;
        h.runtime.bake = None;
        self.set_host_state(ri, hi, HostPhase::Applying, "apply sent");
        self.touch_host(ri, hi);
        self.send_to(ri, hi, server_message::Msg::Apply(apply));
    }

    fn start_bake(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        // Without the run's result (the host reconnected already on the
        // target revision), its plan says what it changed.
        if self.live[ri].hosts[hi].runtime.bake.is_none() {
            let changed = self.plan_changes(ri, hi);
            self.live[ri].hosts[hi].runtime.bake = Some(self.bake_window(ri, hi, changed));
        }
        let bake = self.bake_seconds(ri, hi);
        let why = match &self.live[ri].hosts[hi].runtime.bake {
            Some(w) if bake > w.seconds => "back from its reboot; baking".to_owned(),
            Some(w) => bake::reason(w),
            None => "applied; baking".to_owned(),
        };
        let until = now + chrono::Duration::seconds(bake as i64);
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.bake_until = Some(until);
            h.runtime.failing_since.clear();
            h.runtime.bake_reports = 0;
            h.runtime.bake_requests.clear();
            h.runtime.bake_asked_until = None;
        }
        self.set_host_state(ri, hi, HostPhase::Baking, &why);
        if bake > 0 {
            self.send_bake_request(ri, hi, until);
        }
    }

    /// Row `hi`'s bake window in seconds: what its step resolved to (see
    /// [`bake`]), else its phase's.
    fn bake_seconds(&self, ri: usize, hi: usize) -> u64 {
        let live = &self.live[ri];
        let bake = row_bake(&live.record.runtime.phases, &live.hosts[hi]);
        self.reboot_bake(ri, hi, bake)
    }

    /// The bake window for row `hi`'s step having changed `changed` (None:
    /// unknown, so the phase's): the phase's `bake` raised by the changed
    /// modules' (or a cluster's workloads') own.
    fn bake_window(&self, ri: usize, hi: usize, changed: Option<BTreeSet<String>>) -> BakeWindow {
        let live = &self.live[ri];
        let min = phase_bake(&live.record.runtime.phases, &live.hosts[hi]);
        let Some(changed) = changed else {
            return BakeWindow {
                seconds: min,
                ..Default::default()
            };
        };
        let revision = &live.revision;
        let cluster = live.hosts[hi].runtime.kind == TargetKind::Cluster;
        bake::window(min, &changed, |name| {
            if cluster {
                revision.workload(name).and_then(|w| w.config.bake)
            } else {
                revision.module(name).and_then(|m| m.config.bake)
            }
        })
    }

    /// The modules a successful run changed, from its outcome; from older
    /// agents' results, nothing when it counted no changes, else what the
    /// plan expected.
    fn result_changes(
        &self,
        ri: usize,
        hi: usize,
        result: &ApplyResult,
    ) -> Option<BTreeSet<String>> {
        if let Ok(outcome) =
            serde_json::from_slice::<buckle_core::engine::ApplyOutcome>(&result.outcome_json)
        {
            return Some(bake::changed(&outcome));
        }
        if result.changes == 0 && result.removals == 0 {
            return Some(BTreeSet::new());
        }
        self.plan_changes(ri, hi)
    }

    /// The modules row `hi`'s plan expects to change, when it has one.
    fn plan_changes(&self, ri: usize, hi: usize) -> Option<BTreeSet<String>> {
        let plan = self.live[ri].hosts[hi].plan.as_ref()?;
        serde_json::from_value::<buckle_core::plan::Plan>(plan.clone())
            .ok()
            .map(|p| bake::planned(&p))
    }

    /// A host back from a reboot bakes for at least `settle` (and a minute),
    /// and must report: its run deferred health checks to after the reboot.
    fn reboot_bake(&self, ri: usize, hi: usize, bake: u64) -> u64 {
        let back = self.live[ri].hosts[hi]
            .runtime
            .reboot
            .as_ref()
            .is_some_and(|w| w.back_at.is_some());
        if !back {
            return bake;
        }
        let settle = self.live[ri].record.policy.settle.as_secs();
        bake.max(settle).max(MIN_POST_REBOOT_BAKE)
    }

    /// Records the reboot a host's result announced.
    fn await_reboot(&mut self, ri: usize, hi: usize, reboot: &pb::Reboot) {
        let now = self.now();
        let wait = duration_of(reboot.delay.as_ref()) + duration_of(reboot.timeout.as_ref());
        self.live[ri].hosts[hi].runtime.reboot = Some(RebootWait {
            boot_id: non_empty(&reboot.boot_id),
            requested_by: reboot.requested_by.clone(),
            since: now,
            until: now + wait,
            back_at: None,
        });
        self.touch_host(ri, hi);
    }

    /// The host applied and is about to reboot: wait for it, unless the
    /// result reached the server only after the reboot (on a session from
    /// another boot).
    fn on_reboot_announced(&mut self, ri: usize, hi: usize) {
        let host = self.live[ri].hosts[hi].host.clone();
        let boot = self.boot_ids.get(&host).cloned();
        let Some(wait) = self.live[ri].hosts[hi].runtime.reboot.clone() else {
            return;
        };
        self.live[ri].hosts[hi].runtime.disconnected_at = None;
        let already_back = self.connected(&host)
            && wait.boot_id.is_some()
            && boot.is_some()
            && wait.back_on(boot.as_deref());
        let why = if wait.requested_by.is_empty() {
            "applied; rebooting".to_owned()
        } else {
            format!("applied; rebooting for {}", wait.requested_by.join(", "))
        };
        self.set_host_state(ri, hi, HostPhase::Rebooting, &why);
        if already_back {
            self.back_from_reboot(ri, hi);
        }
    }

    /// The agent is back from a new boot: bake it.
    fn back_from_reboot(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        let (host, phase, rollout, trace, since) = {
            let live = &mut self.live[ri];
            let h = &mut live.hosts[hi];
            let since = h.runtime.reboot.as_mut().map(|w| {
                w.back_at = Some(now);
                w.since
            });
            (
                h.host.clone(),
                h.phase.clone(),
                live.record.id.clone(),
                live.record.runtime.trace_id.clone(),
                since.unwrap_or(now),
            )
        };
        let took = (now - since).num_seconds().max(0);
        self.audit(
            "controller",
            "rollout.host.rebooted",
            &rollout,
            format!("{host}: back from its reboot after {took}s; baking"),
            serde_json::json!({"host": host, "phase": phase, "seconds": took}),
            trace,
        );
        self.start_bake(ri, hi);
    }

    /// The agent could not start the reboot its result announced.
    fn on_reboot_failed(&mut self, host: &str, failed: pb::RebootFailed) {
        let boot = non_empty(&failed.boot_id);
        for ri in 0..self.live.len() {
            let Some(hi) = self.live[ri].find(host) else {
                continue;
            };
            let h = &self.live[ri].hosts[hi];
            if h.state != HostPhase::Rebooting
                || h.runtime
                    .reboot
                    .as_ref()
                    .is_some_and(|w| boot.is_some() && w.back_on(boot.as_deref()))
            {
                continue;
            }
            let why = format!("the reboot did not happen: {}", failed.error);
            self.live[ri].hosts[hi].runtime.error = Some(why.clone());
            self.fail_host(ri, hi, &why);
        }
    }

    /// How long after its bake window a connected host that never sent a
    /// bake report is given before it fails.
    fn bake_report_grace(&self, ri: usize) -> chrono::Duration {
        let settle =
            chrono::Duration::from_std(self.live[ri].record.policy.settle).unwrap_or_default();
        settle.max(chrono::Duration::minutes(1))
    }

    /// Until when row `hi`'s agent must sample its bake: the window and,
    /// while a required check fails, that check's settle (it is followed
    /// until it recovers or counts as a regression).
    fn bake_watch_until(&self, ri: usize, hi: usize) -> Option<DateTime<Utc>> {
        let settle =
            chrono::Duration::from_std(self.live[ri].record.policy.settle).unwrap_or_default();
        let h = &self.live[ri].hosts[hi];
        let failing = h.runtime.failing_since.values().min().map(|s| *s + settle);
        h.runtime.bake_until.into_iter().chain(failing).max()
    }

    fn send_bake_request(&mut self, ri: usize, hi: usize, until: DateTime<Utc>) {
        let span = self.host_span(ri, hi);
        let request_id = new_id();
        self.live[ri].hosts[hi].runtime.bake_asked_until = Some(until);
        {
            let requests = &mut self.live[ri].hosts[hi].runtime.bake_requests;
            requests.push(request_id.clone());
            // A handful is plenty (one per reconnect).
            if requests.len() > 8 {
                requests.remove(0);
            }
        }
        self.touch_host(ri, hi);
        self.send_to(
            ri,
            hi,
            server_message::Msg::Health(RunHealth {
                target: String::new(),
                revision: String::new(),
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
        let rollout = self.live[ri].record.id.clone();
        self.send_to(
            ri,
            hi,
            server_message::Msg::Rollback(pb::Rollback {
                target: String::new(),
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

    /// Row `hi` baked: healthy, unless a later step waits, in which case it
    /// moves on to that step and plans it once its stage opens.
    fn finish_step(&mut self, ri: usize, hi: usize) {
        let Some(next) = self.live[ri].hosts[hi].runtime.next_step().cloned() else {
            self.set_host_state(ri, hi, HostPhase::Healthy, "baked");
            return;
        };
        let (host, from) = {
            let h = &mut self.live[ri].hosts[hi];
            let from = h.runtime.stage;
            h.runtime.step += 1;
            h.runtime.stage = next.stage;
            h.plan_digest = None;
            h.plan = None;
            h.result = None;
            h.wave = None;
            h.runtime.request_id = None;
            h.runtime.bake_until = None;
            h.runtime.bake = None;
            h.runtime.failing_since.clear();
            h.runtime.bake_reports = 0;
            h.runtime.bake_requests.clear();
            h.runtime.bake_asked_until = None;
            h.runtime.approval_digest = None;
            h.runtime.plan_changes = None;
            h.runtime.plan_removals = None;
            h.runtime.destructive = false;
            h.runtime.deferred_modules.clear();
            h.runtime.force = false;
            h.runtime.force_plan = false;
            h.runtime.reboot = None;
            h.runtime.stale_refusals = 0;
            h.runtime.apply_resends = 0;
            h.runtime.preview = false;
            h.runtime.previewed = None;
            h.runtime.drifted_from = None;
            (h.host.clone(), from)
        };
        let (id, phase, trace) = {
            let live = &self.live[ri];
            (
                live.record.id.clone(),
                live.hosts[hi].phase.clone(),
                live.record.runtime.trace_id.clone(),
            )
        };
        self.live[ri].host_spans.remove(&host);
        self.audit(
            "controller",
            "rollout.host.staged",
            &id,
            format!(
                "{host}: stage {} applied and baked; waits for stage {}",
                from + 1,
                next.stage + 1
            ),
            serde_json::json!({"host": host, "phase": phase, "from_stage": from, "stage": next.stage}),
            trace,
        );
        self.set_host_state(
            ri,
            hi,
            HostPhase::PlanPending,
            "its stage is done; the next step plans when its stage opens",
        );
        self.request_plan(ri, hi);
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
                    let window = row_bake(&l.record.runtime.phases, h) > 0
                        || h.runtime
                            .reboot
                            .as_ref()
                            .is_some_and(|w| w.back_at.is_some());
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
                if h.state == HostPhase::Rebooting
                    && let Some(w) = &h.runtime.reboot
                {
                    consider(w.until);
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

    async fn on_time(&mut self) {
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
                    // Past the window, a failing check is followed through
                    // settle, but the agent samples only until the `until`
                    // it was asked for: ask it to cover the rest.
                    let h = &self.live[ri].hosts[hi];
                    if window
                        && h.runtime.bake_until.is_some_and(|u| u <= now)
                        && !h.runtime.failing_since.is_empty()
                        && self.row_connected(ri, hi)
                        && let Some(until) = self.bake_watch_until(ri, hi)
                        && until > now
                        && h.runtime.bake_asked_until.is_none_or(|a| a < until)
                    {
                        self.send_bake_request(ri, hi, until);
                    }
                    let h = &self.live[ri].hosts[hi];
                    let reported = !window || h.runtime.bake_reports > 0;
                    let ended = h.runtime.bake_until.filter(|u| *u <= now);
                    if let Some(until) = ended
                        && h.runtime.failing_since.is_empty()
                    {
                        if reported && (!window || self.row_connected(ri, hi)) {
                            self.finish_step(ri, hi);
                            continue;
                        }
                        if !reported
                            && self.row_connected(ri, hi)
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
                if h.state == HostPhase::Rebooting
                    && let Some(w) = h.runtime.reboot.clone()
                    && w.until <= now
                {
                    let secs = (w.until - w.since).num_seconds().max(0);
                    let why = if self.row_connected(ri, hi) {
                        format!(
                            "still up on the same boot {secs}s after its run asked for a reboot"
                        )
                    } else {
                        format!("did not come back from its reboot within {secs}s")
                    };
                    self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                    self.fail_host(ri, hi, &why);
                    continue;
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
                    && self.row_connected(ri, hi)
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
                    && !self.row_connected(ri, hi)
                {
                    let state = h.state;
                    if h.runtime.kind == TargetKind::Cluster {
                        self.live[ri].hosts[hi].runtime.disconnected_at = None;
                        self.touch_host(ri, hi);
                        self.executor_lost(ri, hi).await;
                        continue;
                    }
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
        self.on_time().await;
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
                    self.drive_stragglers(ri).await;
                }
                self.refresh_display(ri);
            }
            changed |= self.start_next_queued().await;
            if !changed {
                break;
            }
        }
    }

    /// Marks the queued (detected) rollouts among `range` skipped: `newer`
    /// superseded them before they started.
    fn skip_queued(&mut self, range: std::ops::Range<usize>, newer: &str) {
        for ri in range {
            if self.live[ri].record.state == RolloutState::Detected {
                self.live[ri].record.runtime.superseded_by = Some(newer.to_owned());
                self.finish(
                    ri,
                    RolloutState::Skipped,
                    "controller",
                    &format!("superseded by {newer}"),
                );
            }
        }
    }

    /// Starts a queued rollout once none is active. The newest queued
    /// revision's `supersede` policy picks which: the oldest (`sequential`),
    /// or the newest with the rest skipped (`latest`).
    async fn start_next_queued(&mut self) -> bool {
        if self.live.iter().any(|l| l.record.is_active()) {
            return false;
        }
        let queued: Vec<usize> = (0..self.live.len())
            .filter(|&i| self.live[i].record.state == RolloutState::Detected)
            .collect();
        let (Some(&oldest), Some(&newest)) = (queued.first(), queued.last()) else {
            return false;
        };
        let next = match self.live[newest].record.policy.supersede {
            SupersedePolicy::Sequential => oldest,
            SupersedePolicy::Latest | SupersedePolicy::Replace => {
                let newest_id = self.live[newest].record.id.clone();
                self.skip_queued(0..newest, &newest_id);
                newest
            }
        };
        let next_id = self.live[next].record.id.clone();
        if let Err(e) = self.freeze(next, None).await {
            tracing::error!(rollout = %next_id, error = %e, "a queued rollout could not start");
            self.finish(
                next,
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
                // Targets that appeared since the freeze join a phase that
                // has not started yet.
                self.admit_late_targets(ri, None).await;
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
            // The next stage opens once every target of the open one has
            // finished (or will miss the phase).
            if self.advance_stage(ri, p).await {
                changed = true;
            }
            if self.serve_waiting_executors(ri, p, true).await {
                changed = true;
            }
            if self.refresh_previews(ri, p) {
                changed = true;
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
                    && self.stage_open(ri, hi)
                    && self.row_eligible(ri, hi)
                    && !self.row_session(ri, hi).is_some_and(|s| busy.contains(&s))
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
            let done = upto.iter().all(|&i| self.stage_done(ri, i));
            if !done || self.unapproved(ri, p, &hosts) {
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

    /// Whether phase `p` requires approval and none of its hosts has gone
    /// past it (started, finished or been skipped): all are still to plan or
    /// waiting to start. Such a phase never completes by its hosts missing
    /// it, or the phases after it would start without anything in it having
    /// been approved and applied; it waits for its hosts to come back, or
    /// for an operator to skip them or abort.
    fn unapproved(&self, ri: usize, p: usize, hosts: &[usize]) -> bool {
        self.live[ri].record.runtime.phases[p].approval == ApprovalPolicy::Required
            && !hosts.is_empty()
            && hosts.iter().all(|&i| {
                matches!(
                    self.live[ri].hosts[i].state,
                    HostPhase::PlanPending | HostPhase::Planned
                )
            })
    }

    /// Opens the next stage of phase `p` when every target of the open stage
    /// (and below) has finished or will miss the phase: targets still to plan
    /// or apply but unreachable are marked missed, the stage's targets whose
    /// plans waited for it (clusters, later steps) are planned, and clusters
    /// get an executor. Returns whether a stage opened.
    async fn advance_stage(&mut self, ri: usize, p: usize) -> bool {
        let mut opened = false;
        loop {
            let (open, stages) = {
                let phase = &self.live[ri].record.runtime.phases[p];
                (phase.open_stage, phase.stages)
            };
            if open + 1 >= stages {
                return opened;
            }
            let rows = self.live[ri].phase_hosts(p);
            let blocking = rows.iter().any(|&hi| {
                self.live[ri].hosts[hi].runtime.stage <= open && !self.stage_done(ri, hi)
            });
            if blocking {
                return opened;
            }
            for &hi in &rows {
                let h = &self.live[ri].hosts[hi];
                if h.runtime.stage <= open
                    && matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
                {
                    self.set_host_state(
                        ri,
                        hi,
                        HostPhase::Missed,
                        "offline or excluded during its stage",
                    );
                }
            }
            let next = open + 1;
            self.live[ri].record.runtime.phases[p].open_stage = next;
            self.live[ri].dirty = true;
            let (id, name, trace) = {
                let live = &self.live[ri];
                (
                    live.record.id.clone(),
                    live.record.runtime.phases[p].name.clone(),
                    live.record.runtime.trace_id.clone(),
                )
            };
            self.audit(
                "controller",
                "rollout.phase.stage",
                &id,
                format!("phase '{name}': stage {} of {stages} open", next + 1),
                serde_json::json!({"phase": name, "stage": next, "stages": stages}),
                trace,
            );
            self.open_stage_targets(ri, p, next).await;
            opened = true;
        }
    }

    /// Plans the targets of stage `stage` of phase `p` whose plans waited
    /// for it, picking an executor for each cluster first.
    async fn open_stage_targets(&mut self, ri: usize, p: usize, stage: u32) {
        let rows: Vec<usize> = self.live[ri]
            .phase_hosts(p)
            .into_iter()
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.runtime.stage == stage && h.state == HostPhase::PlanPending
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        self.assign_executors(ri, &rows, true).await;
        for hi in rows {
            self.request_plan(ri, hi);
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
                || self.row_session(ri, hi).is_some_and(|s| busy.contains(&s))
                || h.state != HostPhase::Planned
                || self.is_virtual(ri, hi)
                || self.later_phase(ri, hi)
                || !self.stage_open(ri, hi)
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
    /// stragglers stage) is working on, and hosts executing a cluster's
    /// apply in any rollout: they are not started here until that is done.
    fn busy_elsewhere(&self, ri: usize) -> HashSet<String> {
        self.live
            .iter()
            .enumerate()
            .flat_map(|(rj, l)| {
                l.hosts
                    .iter()
                    .filter(|h| h.state.in_flight() || h.state == HostPhase::RollingBack)
                    .flat_map(move |h| {
                        let own = (rj != ri && h.runtime.kind == TargetKind::Host)
                            .then(|| h.host.clone());
                        own.into_iter().chain(h.runtime.executor.clone())
                    })
            })
            .collect()
    }

    /// Another live rollout (running, or stable with its stragglers stage
    /// open) has a row for `host`: it speaks for the agent until that row is
    /// done, so no second rollout admits the host meanwhile.
    fn spoken_for_elsewhere(&self, ri: usize, host: &str) -> bool {
        self.live.iter().enumerate().any(|(rj, l)| {
            rj != ri
                && (l.record.is_active() || l.record.runtime.stragglers_open)
                && l.find(host).is_some_and(|hi| !l.hosts[hi].state.finished())
        })
    }

    fn start_gate(&mut self, ri: usize, p: usize) {
        let targets: Vec<usize> = (0..self.live[ri].hosts.len())
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.state == HostPhase::Healthy && h.runtime.phase_index.is_some_and(|i| i <= p)
            })
            .collect();
        let mut gate = GateRuntime::default();
        let tp = traceparent_of(&self.live[ri].span).unwrap_or_default();
        let now = self.now();
        for hi in targets {
            let rid = new_id();
            let host = self.live[ri].hosts[hi].host.clone();
            gate.pending.insert(host, rid.clone());
            if self.row_connected(ri, hi) {
                self.send_to(
                    ri,
                    hi,
                    server_message::Msg::Health(RunHealth {
                        target: String::new(),
                        revision: String::new(),
                        request_id: rid,
                        scope: HealthScope::Rollout as i32,
                        modules: vec![],
                        until: None,
                        traceparent: tp.clone(),
                    }),
                );
            } else {
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
        // An action rollout promotes nothing and has no stragglers.
        if self.live[ri].record.runtime.action.is_some() {
            self.finish(
                ri,
                RolloutState::Completed,
                "controller",
                "every phase ran the action, baked and passed its gate",
            );
            self.live[ri].record.runtime.end_reason = None;
            return;
        }
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
            stages: 1,
            open_stage: 0,
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
        // Targets that appeared since the freeze, and carried stragglers.
        let candidates = self.load_targets(None).await.unwrap_or_default();
        for (host, facts) in candidates {
            let carried_here = carried.contains(&host.id);
            if self.live[ri].find(&host.id).is_some()
                || host.status != HostStatus::Active
                || host.executor_of().is_some()
            {
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
            self.add_straggler(ri, &host.id, si, host.kind);
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
            h.runtime.error_detail = None;
            h.runtime.failed_checks.clear();
            // The stragglers stage has no stages: one whole step.
            h.runtime.set_steps(Vec::new());
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

    fn add_straggler(&mut self, ri: usize, host: &str, si: usize, kind: TargetKind) {
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
                kind,
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
        // A host a running rollout admitted to one of its phases converges
        // there; taking it to the older stable revision as well would have
        // two rollouts speak for one agent.
        if self.spoken_for_elsewhere(ri, host) {
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
                let Some(rec) = db.hosts().target(&h)? else {
                    return Ok(None);
                };
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                Ok(Some((rec, facts.unwrap_or(serde_json::Value::Null))))
            })
            .await
            .ok()
            .flatten();
        let Some((record, facts)) = found else { return };
        if let Some(c) = &record.cluster {
            self.clusters.insert(record.id.clone(), c.spec.clone());
        }
        if record.status != HostStatus::Active || record.executor_of().is_some() {
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
            self.add_straggler(ri, host, si, record.kind);
        }
    }

    async fn drive_stragglers(&mut self, ri: usize) {
        let Some(si) = self.live[ri].stragglers_index() else {
            return;
        };
        self.serve_waiting_executors(ri, si, false).await;
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
                && self.row_eligible(ri, hi)
                && !busy.contains(&h.host)
                && !self.row_session(ri, hi).is_some_and(|s| busy.contains(&s))
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
                let _ = &host;
                if !attempted && self.row_connected(ri, hi) {
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

    // ------------------------------------------------------------ executors

    /// Picks an executor for each cluster row among `rows` still without
    /// one (and still to plan or apply), returning the rows that got one.
    /// A cluster whose candidates are all mid-wave waits for them; one with
    /// no candidate at all starts the "awaiting executor" grace when `grace`
    /// (inside a phase; the stragglers stage waits without end).
    async fn assign_executors(&mut self, ri: usize, rows: &[usize], grace: bool) -> Vec<usize> {
        let waiting: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.runtime.kind == TargetKind::Cluster
                    && h.runtime.executor.is_none()
                    && matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
            })
            .collect();
        if waiting.is_empty() {
            return Vec::new();
        }
        let Ok(targets) = self.load_targets(None).await else {
            return Vec::new();
        };
        let now = self.now();
        let mut picked = Vec::new();
        for hi in waiting {
            match self.pick_executor(ri, hi, &targets) {
                Pick::Executor(executor) => {
                    self.set_executor(ri, hi, Some(executor));
                    self.live[ri].hosts[hi].runtime.disconnected_at = None;
                    picked.push(hi);
                }
                Pick::Busy => {}
                Pick::None if grace => {
                    let h = &mut self.live[ri].hosts[hi];
                    if h.runtime.disconnected_at.is_none() {
                        h.runtime.disconnected_at = Some(now);
                        self.touch_host(ri, hi);
                    }
                }
                Pick::None => {}
            }
        }
        picked
    }

    /// Picks executors for the cluster rows of phase `p` in its open stage
    /// that wait for one, and asks the newly spoken-for ones for their
    /// plans. Run on every pass, so a host that connects or finishes its
    /// own wave is picked up at once.
    async fn serve_waiting_executors(&mut self, ri: usize, p: usize, grace: bool) -> bool {
        let open = self.live[ri].record.runtime.phases[p].open_stage;
        let rows: Vec<usize> = self.live[ri]
            .phase_hosts(p)
            .into_iter()
            .filter(|&hi| {
                let h = &self.live[ri].hosts[hi];
                h.runtime.kind == TargetKind::Cluster
                    && h.runtime.executor.is_none()
                    && h.runtime.stage == open
            })
            .collect();
        if rows.is_empty() {
            return false;
        }
        let picked = self.assign_executors(ri, &rows, grace).await;
        for hi in &picked {
            if self.live[ri].hosts[*hi].state == HostPhase::PlanPending {
                self.request_plan(ri, *hi);
            }
        }
        !picked.is_empty()
    }

    /// A host agent that may speak for cluster row `hi` now: connected and
    /// active, running an agent with a pipeline per target, matching the
    /// cluster's `executors` selector, and not mid-wave in any rollout. The
    /// cluster's last executor is preferred (sticky), then names sort.
    fn pick_executor(
        &self,
        ri: usize,
        hi: usize,
        targets: &[(crate::store::HostRecord, serde_json::Value)],
    ) -> Pick {
        let executors = self.cluster_executors(ri, hi);
        let target = self.live[ri].hosts[hi].host.as_str();
        let parent = self.clusters.get(target).and_then(|s| s.cluster.as_deref());
        let fleet = &self.live[ri].revision.fleet;
        let busy = self.busy_elsewhere(usize::MAX);
        let inflight: HashSet<&str> = self
            .live
            .iter()
            .flat_map(|l| l.hosts.iter())
            .filter(|h| h.runtime.kind == TargetKind::Host && h.state.in_flight())
            .map(|h| h.host.as_str())
            .collect();
        let able: Vec<&str> = targets
            .iter()
            .filter(|(h, _)| h.kind == TargetKind::Host)
            .filter(|(h, _)| h.status == HostStatus::Active && !self.excluded.contains(&h.id))
            .filter(|(h, _)| self.connected(&h.id))
            .filter(|(h, _)| {
                self.capabilities
                    .get(&h.id)
                    .is_some_and(|c| c.iter().any(|c| c == TARGETS_CAPABILITY))
            })
            .filter(|(h, facts)| match h.executor_of() {
                // An in-cluster executor speaks for its own cluster only.
                Some(c) => c == target || parent == Some(c),
                None => executors.as_ref().is_some_and(|executors| {
                    crate::fleet::record_context(h, facts, fleet).matches(executors)
                }),
            })
            .map(|(h, _)| h.id.as_str())
            .collect();
        if able.is_empty() {
            return Pick::None;
        }
        let mut candidates: Vec<&str> = able
            .into_iter()
            .filter(|h| !busy.contains(*h) && !inflight.contains(h))
            .collect();
        if candidates.is_empty() {
            return Pick::Busy;
        }
        candidates.sort();
        let cluster = self.live[ri].hosts[hi].host.clone();
        let last = self.executors.get(&cluster).cloned();
        candidates
            .iter()
            .find(|c| Some(**c) == last.as_deref())
            .or(candidates.first())
            .map(|c| Pick::Executor((*c).to_owned()))
            .unwrap_or(Pick::None)
    }

    /// Records `executor` as cluster row `hi`'s executor (and the cluster's
    /// last, for stickiness), with an audit entry.
    fn set_executor(&mut self, ri: usize, hi: usize, executor: Option<String>) {
        let cluster = self.live[ri].hosts[hi].host.clone();
        let from = self.live[ri].hosts[hi].runtime.executor.clone();
        if from == executor {
            return;
        }
        self.live[ri].hosts[hi].runtime.executor = executor.clone();
        self.touch_host(ri, hi);
        if let Some(e) = &executor {
            self.executors.insert(cluster.clone(), e.clone());
            self.executors_dirty.insert(cluster.clone());
        }
        let (id, trace) = {
            let live = &self.live[ri];
            (live.record.id.clone(), live.record.runtime.trace_id.clone())
        };
        let message = match (&from, &executor) {
            (_, Some(e)) => format!("{cluster}: {e} speaks for the cluster"),
            (Some(f), None) => format!("{cluster}: {f} no longer speaks for the cluster"),
            (None, None) => return,
        };
        self.audit(
            "controller",
            "rollout.target.executor",
            &id,
            message,
            serde_json::json!({"host": cluster, "executor": executor, "from": from}),
            trace,
        );
    }

    /// Cluster row `hi`'s executor stayed away past the disconnect timeout:
    /// another is picked and the cluster re-planned; with none to pick, the
    /// cluster fails its wave when it was in flight, else waits.
    async fn executor_lost(&mut self, ri: usize, hi: usize) {
        let lost = self.live[ri].hosts[hi].runtime.executor.clone();
        let state = self.live[ri].hosts[hi].state;
        self.set_executor(ri, hi, None);
        let targets = self.load_targets(None).await.unwrap_or_default();
        let picked = match self.pick_executor(ri, hi, &targets) {
            Pick::Executor(e) => Some(e),
            Pick::Busy | Pick::None => None,
        };
        match (picked, state) {
            (Some(executor), _) => {
                self.set_executor(ri, hi, Some(executor));
                if state.in_flight() || state == HostPhase::Planned {
                    let why = format!(
                        "executor {} lost; re-planning through {}",
                        lost.unwrap_or_default(),
                        self.live[ri].hosts[hi]
                            .runtime
                            .executor
                            .clone()
                            .unwrap_or_default()
                    );
                    self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                    self.replan(ri, hi, "controller");
                } else {
                    self.request_plan(ri, hi);
                }
            }
            (None, s) if s.in_flight() || s == HostPhase::RollingBack => {
                let why = format!(
                    "executor {} lost and no other connected host can speak for the cluster",
                    lost.unwrap_or_default()
                );
                self.live[ri].hosts[hi].runtime.error = Some(why.clone());
                if s == HostPhase::RollingBack {
                    self.set_host_state(
                        ri,
                        hi,
                        HostPhase::Failed,
                        "rollback failed; needs attention",
                    );
                } else {
                    self.fail_host(ri, hi, &why);
                }
            }
            (None, _) => {
                // Still to plan or apply when the grace ran out: it misses
                // its phase like an offline host and converges in the
                // stragglers stage, where it waits for an executor without
                // end.
                let in_stragglers =
                    self.live[ri].hosts[hi].runtime.phase_index == self.live[ri].stragglers_index();
                if !in_stragglers
                    && matches!(
                        self.live[ri].hosts[hi].state,
                        HostPhase::PlanPending | HostPhase::Planned
                    )
                {
                    self.set_host_state(
                        ri,
                        hi,
                        HostPhase::Missed,
                        "no connected host could speak for the cluster within the grace",
                    );
                }
            }
        }
    }

    /// The selector choosing the hosts that may speak for cluster row `hi`
    /// (`executors`, else `nodes`), from the cluster's record. A cluster
    /// with neither has no executor and waits.
    fn cluster_executors(&self, ri: usize, hi: usize) -> Option<buckle_core::Filter> {
        let spec = self.clusters.get(&self.live[ri].hosts[hi].host)?;
        buckle_core::Filter::new(spec.executors()?).ok()
    }

    // ------------------------------------------------------------ late admission

    /// Admits targets that appeared since the freeze (enrolled, activated or
    /// created) to a phase of rollout `ri` that has not started yet; the
    /// phase's stages are laid out again around them. Anything matching a
    /// phase already reached waits for the stragglers stage, as before.
    /// Admits targets that appeared since the freeze (`only` that host, or
    /// every host) to a phase the rollout has not reached yet: the first
    /// phase their selector names, when its index is above the current one.
    /// Called when a host connects (its Hello labels are stored by then),
    /// when a host's status or labels change, and when a phase ends.
    async fn admit_late_targets(&mut self, ri: usize, only: Option<&str>) {
        // An action rollout runs on the targets it was started for.
        if !self.live[ri].record.is_running() || self.live[ri].record.runtime.action.is_some() {
            return;
        }
        let current = self.live[ri].record.runtime.current_phase;
        let phases = crate::fleet::effective_phases(&self.live[ri].record.policy);
        if current + 1 >= phases.len() {
            return;
        }
        let revision = self.live[ri].record.revision.clone();
        let candidate = |this: &Self, host: &crate::store::HostRecord| {
            host.status == HostStatus::Active
                && host.executor_of().is_none()
                && this.live[ri].find(&host.id).is_none()
                && !this.live[ri].record.runtime.untargeted.contains(&host.id)
                && host.applied_revision.as_deref() != Some(revision.as_str())
                && !this.spoken_for_elsewhere(ri, &host.id)
        };
        let candidates: Vec<(crate::store::HostRecord, serde_json::Value)> = match only {
            Some(host) => {
                if self.live[ri].find(host).is_some() || self.spoken_for_elsewhere(ri, host) {
                    return;
                }
                let h = host.to_owned();
                let found = self
                    .store
                    .call(move |db| {
                        let Some(rec) = db.hosts().target(&h)? else {
                            return Ok(None);
                        };
                        let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                        Ok(Some((rec, facts.unwrap_or(serde_json::Value::Null))))
                    })
                    .await;
                match found {
                    Ok(Some(found)) => vec![found],
                    _ => return,
                }
            }
            None => {
                let rev = self.live[ri].revision.clone();
                match self.load_targets(Some(rev)).await {
                    Ok(targets) => targets,
                    Err(_) => return,
                }
            }
        };
        let now = self.now();
        let id = self.live[ri].record.id.clone();
        let mut touched: BTreeSet<usize> = BTreeSet::new();
        for (host, facts) in &candidates {
            if !candidate(self, host) {
                continue;
            }
            let ctx = crate::fleet::record_context(host, facts, &self.live[ri].revision.fleet);
            let Some(pi) = crate::fleet::phase_for(&ctx, &phases) else {
                continue;
            };
            if pi <= current {
                continue;
            }
            self.live[ri].hosts.push(RolloutHostRecord {
                rollout: id.clone(),
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
                    revision: revision.clone(),
                    phase_index: Some(pi),
                    kind: host.kind,
                    ..Default::default()
                },
            });
            let hi = self.live[ri].hosts.len() - 1;
            self.live[ri].dirty_hosts.insert(hi);
            let trace = self.live[ri].record.runtime.trace_id.clone();
            let phase = phases[pi].name.clone();
            self.audit(
                "controller",
                "rollout.target.admitted",
                &id,
                format!("{} joined phase '{phase}' after the freeze", host.id),
                serde_json::json!({"host": host.id, "phase": phase}),
                trace,
            );
            touched.insert(pi);
        }
        if touched.is_empty() {
            return;
        }
        // Stages are laid out over the whole phase (a cluster's nodes among
        // them), so they need every target, not just the one admitted.
        let targets = match only {
            Some(_) => match self.load_targets(None).await {
                Ok(targets) => targets,
                Err(_) => candidates,
            },
            None => candidates,
        };
        for pi in touched {
            self.compute_stages(ri, pi, &targets);
            for hi in self.live[ri].phase_hosts(pi) {
                if self.live[ri].hosts[hi].state == HostPhase::PlanPending
                    && self.live[ri].hosts[hi].plan_digest.is_none()
                {
                    self.request_plan(ri, hi);
                }
            }
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
        let runs = std::mem::take(&mut self.new_runs);
        let stable = self.stable_dirty.then(|| self.stable.clone()).flatten();
        let cleared = self.pending_clear.clone();
        let executors: Vec<(String, String)> = self
            .executors_dirty
            .iter()
            .filter_map(|c| self.executors.get(c).map(|e| (c.clone(), e.clone())))
            .collect();
        if !(rollouts.is_empty()
            && hosts.is_empty()
            && audits.is_empty()
            && runs.is_empty()
            && stable.is_none()
            && cleared.is_empty()
            && executors.is_empty())
        {
            let (r2, h2, a2, n2) = (
                rollouts.clone(),
                hosts.clone(),
                audits.clone(),
                runs.clone(),
            );
            let result = self
                .store
                .transaction(move |db| {
                    for r in &r2 {
                        db.rollouts().save(r)?;
                    }
                    for h in &h2 {
                        db.rollouts().save_host(h)?;
                    }
                    for run in &n2 {
                        db.runs().add(run)?;
                    }
                    if let Some(s) = &stable {
                        db.kv().set(keys::STABLE_REVISION, s)?;
                    }
                    for key in &cleared {
                        db.kv().delete(key)?;
                    }
                    for (cluster, executor) in &executors {
                        db.kv()
                            .set(&format!("{}{cluster}", keys::EXECUTOR_PREFIX), executor)?;
                    }
                    let mut entries = Vec::new();
                    for a in &a2 {
                        entries.push(db.audit().add(a)?);
                    }
                    // The run each changed host is on, for its live row.
                    let mut run_ids = std::collections::BTreeMap::new();
                    for rollout in h2
                        .iter()
                        .map(|h| h.rollout.as_str())
                        .collect::<BTreeSet<_>>()
                    {
                        for (host, id) in db.runs().ids_for_rollout(rollout)? {
                            run_ids.insert((rollout.to_owned(), host), id);
                        }
                    }
                    Ok((entries, run_ids))
                })
                .await;
            match result {
                Ok((entries, run_ids)) => {
                    self.flush_retry = None;
                    self.flush_failures = 0;
                    self.pending_clear.clear();
                    self.executors_dirty.clear();
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
                            let mut host = view::host(&l.record, h, &l.approvals);
                            host.run_id =
                                run_ids.get(&(h.rollout.clone(), h.host.clone())).cloned();
                            self.events.publish(LiveEvent::RolloutHostChanged {
                                rollout_id: h.rollout.clone(),
                                host,
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
                    let mut kept = runs;
                    kept.append(&mut self.new_runs);
                    self.new_runs = kept;
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
                    if !self.link.send(&host, *m) {
                        undeliverable.insert(host);
                    }
                }
                Out::AssignAndPlan {
                    session,
                    target,
                    revision,
                    stable,
                } => {
                    if !self
                        .assign_and_plan(&session, &target, &revision, stable)
                        .await
                    {
                        undeliverable.insert(session);
                    }
                }
                Out::Targeted {
                    session,
                    target,
                    msg,
                } => {
                    if !self.send_targeted(&session, &target, *msg).await {
                        undeliverable.insert(session);
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

    /// Sends Assign and RequestPlan over `host`'s session, for the host
    /// itself or for the cluster `target` it executes; `false` when the host
    /// has no session.
    async fn assign_and_plan(
        &mut self,
        host: &str,
        target: &str,
        revision: &str,
        stable: bool,
    ) -> bool {
        let Some(assign) = self.assign_message(host, target, revision, stable).await else {
            return true;
        };
        let select = self
            .live
            .iter()
            .enumerate()
            .find_map(|(ri, l)| {
                l.find_target(host, target)
                    .filter(|&hi| l.hosts[hi].runtime.revision == revision)
                    .map(|hi| self.row_select(ri, hi))
            })
            .unwrap_or_default();
        let assigned = self.link.send(host, assign);
        let planned = self.link.send(
            host,
            msg(server_message::Msg::Plan(RequestPlan {
                target: target.to_owned(),
                revision: revision.to_owned(),
                select,
            })),
        );
        assigned && planned
    }

    /// Sends `m` for the cluster `target` over `session`, behind a targeted
    /// Assign of the revision it concerns (for health, the cluster's applied
    /// revision, which the request then names); `false` when the session is
    /// gone.
    async fn send_targeted(&mut self, session: &str, target: &str, mut m: ServerMessage) -> bool {
        let revision = match &m.msg {
            Some(server_message::Msg::Plan(p)) => p.revision.clone(),
            Some(server_message::Msg::Apply(a)) => a.revision.clone(),
            Some(server_message::Msg::Rollback(r)) => r.revision.clone(),
            Some(server_message::Msg::Action(a)) => a.revision.clone(),
            Some(server_message::Msg::Health(_)) => {
                let t = target.to_owned();
                self.store
                    .call(move |db| Ok(db.hosts().target(&t)?.and_then(|r| r.applied_revision)))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        if let Some(server_message::Msg::Health(h)) = &mut m.msg {
            h.revision = revision.clone();
        }
        if !revision.is_empty() {
            let stable = self.stable.as_deref() == Some(revision.as_str());
            if let Some(assign) = self
                .assign_message(session, target, &revision, stable)
                .await
                && !self.link.send(session, assign)
            {
                return false;
            }
        }
        self.link.send(session, m)
    }

    /// The Assign of `revision` for `host` itself (empty `target`) or for
    /// the cluster `target` it executes: for a cluster, how to reach it and
    /// the secrets its workloads reference. `None` when the revision or a
    /// record cannot be loaded (logged).
    async fn assign_message(
        &mut self,
        host: &str,
        target: &str,
        revision: &str,
        stable: bool,
    ) -> Option<ServerMessage> {
        let rev = match self.revisions.load(revision).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(revision = %revision, error = %e, "the revision to assign could not be loaded");
                return None;
            }
        };
        let h = if target.is_empty() { host } else { target }.to_owned();
        let rid = revision.to_owned();
        let found = self
            .store
            .call(move |db| {
                let rec = db.hosts().target(&h)?;
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                let digest = db.revisions().get(&rid)?.and_then(|r| r.tarball_digest);
                Ok((rec, facts.unwrap_or(serde_json::Value::Null), digest))
            })
            .await;
        let Ok((Some(record), facts, tree_digest)) = found else {
            return None;
        };
        let ctx = crate::fleet::record_context(&record, &facts, &rev.fleet);
        let (secrets, cluster) = if target.is_empty() {
            (self.secrets.for_host(&rev, &ctx).await, None)
        } else {
            let Some(spec) = record.cluster.as_ref().map(|c| &c.spec) else {
                tracing::warn!(target = %target, "not a cluster target; nothing assigned");
                return None;
            };
            // The revision's declaration wins over the stored record, which
            // follows the reference revision.
            let declared = rev.fleet.fleet.clusters.get(target);
            let kind = declared
                .map(|d| d.kind.as_str())
                .unwrap_or(crate::targets::core_kind(spec.kind).as_str())
                .to_owned();
            let api = declared
                .map(|d| d.api.clone())
                .unwrap_or_else(|| spec.api.clone())
                .unwrap_or_default();
            let identity = declared
                .map(|d| d.identity.to_string())
                .unwrap_or_else(|| spec.identity.clone());
            let secret = buckle_core::model::ClusterIdentity::parse(&identity)
                .ok()
                .and_then(|i| i.secret().map(str::to_owned));
            let secrets = self
                .secrets
                .for_cluster(&rev, &ctx, secret.as_deref())
                .await;
            let cluster = pb::ClusterTarget {
                kind,
                api,
                identity,
                labels: record.effective_labels().into_iter().collect(),
                fingerprint_key: self.secrets.fingerprint_key(target),
            };
            (secrets, Some(cluster))
        };
        Some(msg(server_message::Msg::Assign(Assign {
            target: target.to_owned(),
            revision: revision.to_owned(),
            stable,
            secrets: Some(secrets),
            tree_digest: tree_digest.unwrap_or_default(),
            cluster,
        })))
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

/// The structured error of a failed ApplyResult: the agent's run-level
/// `error_detail`, else (an agent that sends none) the first failing
/// module's from the outcome, parsed this once.
fn error_view(e: &pb::ErrorDetail) -> buckle_api::ErrorView {
    buckle_api::ErrorView {
        kind: if e.kind == "system" {
            buckle_api::ErrorKind::System
        } else {
            buckle_api::ErrorKind::User
        },
        message: e.message.clone(),
        causes: e.causes.clone(),
        advice: e.advice.clone(),
    }
}

fn result_error_detail(result: &ApplyResult) -> Option<buckle_api::ErrorView> {
    if let Some(e) = &result.error_detail {
        return Some(error_view(e));
    }
    serde_json::from_slice::<buckle_core::engine::ApplyOutcome>(&result.outcome_json)
        .ok()
        .and_then(|o| crate::views::outcome_error(&o))
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
