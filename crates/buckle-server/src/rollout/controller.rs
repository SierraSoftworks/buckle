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
use std::collections::{BTreeSet, HashMap, HashSet};
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
}

impl Default for ControllerOptions {
    fn default() -> Self {
        Self {
            disconnect_timeout: Duration::from_secs(600),
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

fn stopped() -> buckle_core::Error {
    buckle_core::error::system(
        "The rollout controller is not running.",
        &["The server is shutting down or the controller crashed; check the server logs."],
    )
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
        traceparent: String,
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
                let excluded: HashSet<String> = db
                    .hosts()
                    .list()?
                    .into_iter()
                    .filter(|h| h.status != HostStatus::Active)
                    .map(|h| h.id)
                    .collect();
                Ok((out, db.kv().get(keys::STABLE_REVISION)?, excluded))
            })
            .await?;
        let now = deps.clock.now();
        let mut live = Vec::new();
        for (record, mut hosts, approvals) in records {
            let revision = match deps.revisions.load(&record.revision).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(rollout = %record.id, error = %e, "a live rollout's revision could not be loaded; leaving it as it is");
                    continue;
                }
            };
            // Every session is gone after a restart: start the disconnect clock
            // for hosts that are waiting on an agent.
            let mut dirty_hosts = BTreeSet::new();
            for (i, h) in hosts.iter_mut().enumerate() {
                if (h.state.in_flight() || h.state == HostPhase::RollingBack)
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
        }
        let message = if why.is_empty() {
            format!("{host}: {} → {}", from.as_str(), state.as_str())
        } else {
            format!("{host}: {} → {} ({why})", from.as_str(), state.as_str())
        };
        tracing::info!(buckle.rollout = %rollout, buckle.host = %host, buckle.phase = %phase, from = from.as_str(), to = state.as_str(), "rollout host transition");
        self.audit(
            "controller",
            &format!("rollout.host.{}", state.as_str()),
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
                self.flush().await;
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
                HostPhase::Baseline => self.send_baseline(ri, hi),
                HostPhase::Applying => {
                    if c.applied_revision.as_deref() == Some(target.as_str()) {
                        self.start_bake(ri, hi);
                    } else {
                        self.send_apply(ri, hi);
                    }
                }
                HostPhase::Baking => {
                    let until = self.live[ri].hosts[hi].runtime.bake_until.unwrap_or(now);
                    if until > now {
                        self.send_bake_request(ri, hi, until);
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
            if state.in_flight() || state == HostPhase::RollingBack || gate_pending {
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
            if h.runtime.revision != plan.revision {
                continue;
            }
            if !matches!(h.state, HostPhase::PlanPending | HostPhase::Planned) {
                continue;
            }
            if !plan.error.is_empty() {
                let error = format!("planning failed: {}", plan.error);
                self.live[ri].hosts[hi].runtime.error = Some(error.clone());
                self.fail_host(ri, hi, &error);
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
                h.runtime.error = None;
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
                HostPhase::Applying if result.revision == target => {
                    self.live[ri].hosts[hi].result =
                        serde_json::to_value(ResultView::from(&result)).ok();
                    if result.stale_plan {
                        self.live[ri].hosts[hi].plan_digest = None;
                        self.set_host_state(
                            ri,
                            hi,
                            HostPhase::PlanPending,
                            "stale plan; re-planning",
                        );
                        self.request_plan(ri, hi);
                    } else if result.succeeded {
                        self.start_bake(ri, hi);
                    } else {
                        {
                            let h = &mut self.live[ri].hosts[hi];
                            h.runtime.failed_module = (!result.failed_module.is_empty())
                                .then(|| result.failed_module.clone());
                            h.runtime.failed_checks = result.failed_checks.clone();
                            h.runtime.error = Some(if result.error.is_empty() {
                                "apply failed".into()
                            } else {
                                result.error.clone()
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
                HostPhase::RollingBack
                    if prev.as_deref() == Some(result.revision.as_str())
                        || prev.as_deref() == Some(result.applied_revision.as_str()) =>
                {
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
                    self.live[ri].record.runtime.phases[pi].gate = None;
                    self.live[ri].dirty = true;
                    if let Some(hi) = self.live[ri].find(host) {
                        self.live[ri].hosts[hi].runtime.failed_checks = failures.clone();
                        self.touch_host(ri, hi);
                    }
                    self.pause(
                        ri,
                        "gate",
                        format!(
                            "pre-phase gate after '{phase_name}': {host} is failing {}",
                            failures.join(", ")
                        ),
                        Some(host.to_owned()),
                    );
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
                (HealthScope::Bake, HostPhase::Baking) => {
                    self.live[ri].hosts[hi].health =
                        serde_json::to_value(HealthView::from(&report)).ok();
                    let h = &mut self.live[ri].hosts[hi];
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
            let why = format!(
                "rolled itself back from {} to {}: {}",
                short(&lr.from_revision),
                short(&lr.to_revision),
                lr.reason
            );
            {
                let h = &mut self.live[ri].hosts[hi];
                h.runtime.error = Some(why.clone());
                h.runtime.failed_checks = lr
                    .evidence
                    .iter()
                    .filter(|r| !r.passed)
                    .map(check_name)
                    .collect();
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
        if let Some(r) = record
            && r.status == HostStatus::Active
            && self.connected(host)
        {
            self.on_connected(Connected {
                host: host.to_owned(),
                applied_revision: r.applied_revision.clone(),
                last_plan_digest: r.last_plan_digest.clone(),
            })
            .await;
        }
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
                // Accept the failures that caused the pause.
                let p = self.live[ri].record.runtime.current_phase;
                if p < self.live[ri].record.runtime.phases.len() {
                    let hosts = self.live[ri].phase_hosts(p);
                    let failures = hosts
                        .iter()
                        .filter(|&&i| self.live[ri].hosts[i].state == HostPhase::Failed)
                        .count() as u32;
                    let phase = &mut self.live[ri].record.runtime.phases[p];
                    let budget = phase.failure_budget(hosts.len());
                    if failures > budget {
                        phase.accepted_failures += failures - budget;
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
        let tp = traceparent_of(&self.live[ri].span).unwrap_or_default();
        self.outbox.push(Out::AssignAndPlan {
            host,
            revision,
            stable,
            traceparent: tp,
        });
    }

    fn replan(&mut self, ri: usize, hi: usize, actor: &str) {
        {
            let h = &mut self.live[ri].hosts[hi];
            h.runtime.failed_checks.clear();
            h.runtime.failed_module = None;
            h.runtime.error = None;
            h.runtime.failing_since.clear();
            h.runtime.request_id = None;
            h.runtime.bake_until = None;
            h.runtime.attempts += 1;
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
    /// → Rolling: resume or retry host"); a failure-budget breach that still
    /// stands will pause it again on the next failure.
    fn unpause_if_cleared(&mut self, ri: usize, actor: &str) {
        if self.live[ri].record.state == RolloutState::Paused {
            self.unpause(ri, actor);
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
            .filter(|h| h.runtime.applied)
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

    fn send_apply(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        let span = self.host_span(ri, hi);
        let live = &mut self.live[ri];
        let h = &mut live.hosts[hi];
        h.runtime.applied = true;
        h.runtime.apply_started_at = Some(now);
        let digest = h.plan_digest.clone().unwrap_or_default();
        let phase = h.phase.clone();
        let rollout = live.record.id.clone();
        let apply = Apply {
            revision: h.runtime.revision.clone(),
            plan_digest: digest.clone(),
            approval_token: approval_token(&self.key, &rollout, &phase, &digest),
            waived_checks: h.runtime.waived_checks.clone(),
            rollout_id: rollout,
            phase,
            force: h.runtime.force,
            traceparent: traceparent_of(&span).unwrap_or_default(),
        };
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
        }
        self.set_host_state(ri, hi, HostPhase::Baking, "applied; baking");
        if bake > 0 {
            self.send_bake_request(ri, hi, until);
        }
    }

    fn send_bake_request(&mut self, ri: usize, hi: usize, until: DateTime<Utc>) {
        let span = self.host_span(ri, hi);
        let host = self.live[ri].hosts[hi].host.clone();
        self.send(
            &host,
            server_message::Msg::Health(RunHealth {
                request_id: new_id(),
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
        let hosts = self.live[ri].phase_hosts(pi);
        let failures = hosts
            .iter()
            .filter(|&&i| self.live[ri].hosts[i].state == HostPhase::Failed)
            .count() as u32;
        let phase = &self.live[ri].record.runtime.phases[pi];
        let budget = phase.failure_budget(hosts.len());
        if failures <= budget {
            return;
        }
        let host = self.live[ri].hosts[hi].host.clone();
        let reason = format!(
            "phase '{}': {failures} failed host(s) exceed max_failures {} (latest: {host}: {why})",
            phase.name,
            view::parallel_str(phase.max_failures)
        );
        match self.live[ri].record.policy.on_failure {
            FailurePolicy::Pause => self.pause(ri, "failure_budget", reason, Some(host)),
            FailurePolicy::Rollback if self.live[ri].record.runtime.previous_stable.is_some() => {
                self.begin_rollback(ri, "controller")
            }
            FailurePolicy::Rollback => self.pause(ri, "failure_budget", reason, Some(host)),
            FailurePolicy::Continue => {}
        }
    }

    // ------------------------------------------------------------ timers

    fn next_deadline(&self) -> Option<DateTime<Utc>> {
        let settle =
            |l: &Live| chrono::Duration::from_std(l.record.policy.settle).unwrap_or_default();
        let timeout =
            chrono::Duration::from_std(self.options.disconnect_timeout).unwrap_or_default();
        let mut next: Option<DateTime<Utc>> = None;
        let mut consider = |t: DateTime<Utc>| {
            next = Some(next.map_or(t, |n| n.min(t)));
        };
        for l in &self.live {
            for h in &l.hosts {
                if h.state == HostPhase::Baking {
                    for since in h.runtime.failing_since.values() {
                        consider(*since + settle(l));
                    }
                    if let Some(u) = h.runtime.bake_until {
                        consider(u);
                    }
                }
                if let Some(d) = h.runtime.disconnected_at {
                    consider(d + timeout);
                }
            }
        }
        next
    }

    fn on_time(&mut self) {
        let now = self.now();
        let timeout =
            chrono::Duration::from_std(self.options.disconnect_timeout).unwrap_or_default();
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
                    if h.runtime.bake_until.is_some_and(|u| u <= now)
                        && h.runtime.failing_since.is_empty()
                    {
                        self.set_host_state(ri, hi, HostPhase::Healthy, "baked");
                        continue;
                    }
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
                        self.live[ri].record.runtime.phases[pi].gate = None;
                        self.live[ri].dirty = true;
                        self.pause(
                            ri,
                            "gate",
                            format!("pre-phase gate after '{name}': {host} {why}"),
                            Some(host.clone()),
                        );
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
                } else if state == RolloutState::RollingBack {
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
            let hosts = self.live[ri].phase_hosts(p);
            let cap = self.live[ri].record.runtime.phases[p]
                .parallel
                .resolve(hosts.len());
            let mut inflight = hosts
                .iter()
                .filter(|&&i| self.live[ri].hosts[i].state.in_flight())
                .count();
            for &hi in &hosts {
                if inflight >= cap {
                    break;
                }
                let h = &self.live[ri].hosts[hi];
                if h.state == HostPhase::Planned && self.eligible(&h.host) && self.approved(ri, hi)
                {
                    self.start_baseline(ri, hi);
                    inflight += 1;
                    changed = true;
                }
            }
            let done = hosts.iter().all(|&i| {
                let h = &self.live[ri].hosts[i];
                h.state.finished()
                    || (matches!(h.state, HostPhase::PlanPending | HostPhase::Planned)
                        && !self.eligible(&h.host))
            });
            if !done {
                return changed;
            }
            for &hi in &hosts {
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
            if matches!(
                self.live[ri].hosts[hi].state,
                HostPhase::Missed | HostPhase::PlanPending | HostPhase::Planned
            ) {
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
                .filter(|&i| self.live[ri].hosts[i].runtime.applied)
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
                    .any(|&i| self.live[ri].hosts[i].runtime.applied)
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

    async fn flush(&mut self) {
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
        if !(rollouts.is_empty() && hosts.is_empty() && audits.is_empty() && stable.is_none()) {
            let (r2, h2) = (rollouts.clone(), hosts.clone());
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
                    let mut entries = Vec::new();
                    for a in &audits {
                        entries.push(db.audit().add(a)?);
                    }
                    Ok(entries)
                })
                .await;
            match result {
                Ok(entries) => {
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
                    tracing::error!(error = %e, "the rollout checkpoint could not be written; will retry");
                }
            }
        }
        let outbox = std::mem::take(&mut self.outbox);
        for out in outbox {
            match out {
                Out::Msg(host, m) => {
                    self.link.send(&host, m);
                }
                Out::AssignAndPlan {
                    host,
                    revision,
                    stable,
                    traceparent,
                } => {
                    self.assign_and_plan(&host, &revision, stable, traceparent)
                        .await
                }
            }
        }
        // Forget finished rollouts (they stay in the store).
        self.live
            .retain(|l| !l.record.is_terminal() || l.record.runtime.stragglers_open);
    }

    async fn assign_and_plan(
        &mut self,
        host: &str,
        revision: &str,
        stable: bool,
        traceparent: String,
    ) {
        let rev = match self.revisions.load(revision).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(revision = %revision, error = %e, "the revision to assign could not be loaded");
                return;
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
            return;
        };
        let ctx = crate::fleet::record_context(&record, &facts, &rev.fleet);
        let secrets = self.secrets.for_host(&rev, &ctx).await;
        self.link.send(
            host,
            msg(server_message::Msg::Assign(Assign {
                revision: revision.to_owned(),
                stable,
                secrets: Some(secrets),
                tree_digest: tree_digest.unwrap_or_default(),
            })),
        );
        self.link.send(
            host,
            msg(server_message::Msg::Plan(RequestPlan {
                revision: revision.to_owned(),
                select: String::new(),
                traceparent,
            })),
        );
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
    stale_plan: bool,
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
            stale_plan: r.stale_plan,
            outcome: serde_json::from_slice(&r.outcome_json).ok(),
        }
    }
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
            results: r
                .results
                .iter()
                .map(|x| {
                    serde_json::json!({
                        "module": x.module,
                        "check": x.check,
                        "kind": x.kind,
                        "required": x.required,
                        "passed": x.passed,
                        "unknown": x.unknown,
                        "reason": x.reason,
                    })
                })
                .collect(),
        }
    }
}
