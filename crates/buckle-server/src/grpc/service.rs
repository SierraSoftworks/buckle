//! The `Fleet` service.

use super::auth::{self, Whois};
use super::guard::SessionGuard;
use super::tls::ServerIdentity;
use crate::clock::Clock;
use crate::config::{AgentAuth, AgentsConfig, Registration};
use crate::events::EventBus;
use crate::rollout::{Connected, ControllerHandle, HostLink, Sessions};
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{HealthEventRecord, NewAudit, NewHost, RunRecord, Store};
use buckle_api::{Connection as LinkState, HealthSummary, HostStatus, LiveEvent, RevisionStatus};
use buckle_proto::v1::fleet_server::{Fleet, FleetServer};
use buckle_proto::v1::{
    Ack, AgentMessage, Chunk, FetchBlobRequest, FetchRevisionRequest, FetchSecretsRequest, Hello,
    Rejected, SecretBundle, ServerMessage, Welcome, agent_message, server_message,
};
use futures::{Stream, StreamExt};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

const CHUNK: usize = 1024 * 1024;

type ServerStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;
type ChunkStream = Pin<Box<dyn Stream<Item = Result<Chunk, Status>> + Send>>;

/// Shared state of the gRPC front.
#[derive(Clone)]
pub struct FleetService {
    pub store: Store,
    pub sessions: Arc<Sessions>,
    pub controller: ControllerHandle,
    pub revisions: Arc<RevisionCache>,
    pub secrets: ServerSecrets,
    pub agents: AgentsConfig,
    pub whois: Option<Arc<dyn Whois>>,
    pub events: EventBus,
    pub clock: Arc<dyn Clock>,
    pub server_version: String,
    /// Live-session cancellation, enrolment throttling and the Hello timeout.
    pub guard: Arc<SessionGuard>,
    /// Agent upgrades requested from the UI or API.
    pub upgrades: Arc<crate::upgrades::AgentUpgrades>,
    /// Join commands whose tokens enrol new hosts.
    pub joins: Arc<crate::join::Joins>,
}

impl std::fmt::Debug for FleetService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetService").finish_non_exhaustive()
    }
}

/// The largest agent message the server decodes (tonic's default is 4 MiB):
/// an ApplyResult of a host with thousands of steps must still arrive whole.
pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Serves `service` on `listener` until `cancel` trips.
pub async fn serve(
    service: FleetService,
    listener: tokio::net::TcpListener,
    identity: Option<ServerIdentity>,
    cancel: CancellationToken,
) -> buckle_core::Result<()> {
    let mut builder = tonic::transport::Server::builder();
    if let Some(id) = identity {
        builder = builder
            .tls_config(tonic::transport::ServerTlsConfig::new().identity(
                tonic::transport::Identity::from_pem(id.cert_pem, id.key_pem),
            ))
            .map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    "The gRPC TLS configuration is invalid.",
                    &["Check listen.tls in server.yaml."],
                )
            })?;
    }
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    builder
        // An ApplyResult carries the whole outcome (every module and step) and
        // is the authoritative record of a run: never refuse one for its size.
        .add_service(FleetServer::new(service).max_decoding_message_size(MAX_AGENT_MESSAGE_BYTES))
        .serve_with_incoming_shutdown(incoming, cancel.cancelled_owned())
        .await
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The gRPC server stopped unexpectedly.",
                &["Check the server logs."],
            )
        })
}

/// How a session was authenticated.
#[derive(Debug)]
struct Authenticated {
    host: String,
    issued_token: Option<String>,
    status: HostStatus,
}

fn rejected(reason: impl Into<String>, retry: bool) -> ServerStream {
    let m = ServerMessage {
        msg: Some(server_message::Msg::Rejected(Rejected {
            reason: reason.into(),
            retry,
        })),
    };
    Box::pin(futures::stream::once(async move { Ok(m) }))
}

/// How long a session waits for the controller to save its reaction to a
/// durable report before giving up on acknowledging it (the agent re-sends
/// it after its next Welcome).
const DURABLE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn internal(e: buckle_core::Error) -> Status {
    tracing::error!(error = %e, "a gRPC request failed");
    Status::internal("the server could not complete the request")
}

impl FleetService {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.clock.now()
    }

    fn uses(&self, method: AgentAuth) -> bool {
        self.agents.auth.contains(&method)
    }

    fn initial_status(&self) -> HostStatus {
        match self.agents.registration {
            Registration::Auto => HostStatus::Active,
            Registration::Approve => HostStatus::Pending,
        }
    }

    /// Audits how an agent settled an upgrade request.
    async fn settled(&self, host: &str, settled: crate::upgrades::Settled) {
        use crate::upgrades::Settled;
        let (action, message) = match settled {
            Settled::Upgraded { version } => {
                tracing::info!(buckle.host = %host, %version, "the agent upgraded");
                (
                    "host.upgraded",
                    format!("{host} upgraded its agent to {version}"),
                )
            }
            Settled::UpToDate { version } => (
                "host.upgraded",
                format!("{host}'s agent already runs {version}"),
            ),
            Settled::Failed { version, error } => {
                tracing::warn!(buckle.host = %host, %version, %error, "the agent upgrade failed");
                (
                    "host.upgrade_failed",
                    format!("{host} could not upgrade its agent to {version}: {error}"),
                )
            }
        };
        self.audit(host, action, host, message).await;
    }

    async fn audit(&self, actor: &str, action: &str, subject: &str, message: String) {
        let entry = NewAudit {
            at: self.now(),
            actor: actor.to_owned(),
            action: action.to_owned(),
            outcome: "ok".into(),
            subject: Some(subject.to_owned()),
            message,
            detail: None,
            trace_id: None,
        };
        if let Ok(e) = self.store.call(move |db| db.audit().add(&entry)).await {
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

    async fn publish_host(&self, id: &str) {
        if let Some(mut host) = crate::views::load_summary(&self.store, &self.revisions, id).await {
            host.upgrade = self.upgrades.get(id);
            self.events.publish(LiveEvent::HostChanged { host });
        }
    }

    /// Authenticates a session from its Hello. `Err` carries the rejection.
    async fn authenticate(
        &self,
        peer: Option<SocketAddr>,
        hello: &Hello,
        metadata_token: Option<String>,
    ) -> Result<Authenticated, (String, bool)> {
        // A whois that errored says nothing about the agent: the server could
        // not ask (tailscaled restarted, its socket went stale). Whatever the
        // token path then says, the agent is told to retry on its normal
        // backoff rather than the slow one for refused hosts.
        let mut whois_error = None;
        if self.uses(AgentAuth::Tailscale)
            && let (Some(peer), Some(whois)) = (peer, &self.whois)
            && whois.covers(&peer)
        {
            match whois.whois(&peer).await {
                Ok(Some(identity)) => {
                    if !hello.host_id.is_empty() && hello.host_id != identity.node {
                        return Err((
                            format!(
                                "This agent claims to be '{}', but the tailnet says it is '{}'.",
                                hello.host_id, identity.node
                            ),
                            false,
                        ));
                    }
                    // Whois says who the peer is, not that it belongs to the
                    // fleet: nodes shared in from other tailnets reach us too.
                    let trust = &self.agents.tailscale;
                    let admitted = trust.admits(&identity.tags, identity.user.as_deref());
                    if trust.is_configured() && !admitted {
                        return Err((
                            format!(
                                "The tailnet node '{}' is not trusted as an agent; give it one of agents.tailscale.allowed_tags or have one of agents.tailscale.allowed_users own it.",
                                identity.node
                            ),
                            false,
                        ));
                    }
                    let id = identity.node.clone();
                    let now = self.now();
                    // A join command an admin created is that admin's
                    // approval: it skips `registration: approve`.
                    let joined = (!hello.token.is_empty())
                        .then(|| self.joins.redeem(&hello.token, &identity.node, now))
                        .flatten();
                    let status = if joined.is_some() {
                        HostStatus::Active
                    } else if admitted {
                        self.initial_status()
                    } else {
                        HostStatus::Pending
                    };
                    let created = self
                        .store
                        .call(move |db| match db.hosts().get(&id)? {
                            // Enrolled with a token: the token is its identity,
                            // not whichever tailnet node carries its name.
                            Some(h) if h.token_hash.is_some() => Ok(None),
                            Some(h) => Ok(Some((h.status, false))),
                            None => {
                                db.hosts().create(&NewHost {
                                    id: id.clone(),
                                    status,
                                    token_hash: None,
                                    auth_method: "tailscale".into(),
                                    at: now,
                                })?;
                                Ok(Some((status, true)))
                            }
                        })
                        .await
                        .map_err(|e| (e.description().to_string(), true))?;
                    if let Some(j) = &joined
                        && !created.is_some_and(|c| c.1)
                    {
                        // Not a new host: the token stays for the one it was made for.
                        self.joins.release(&j.ticket);
                    }
                    let Some(created) = created else {
                        tracing::debug!(host = %identity.node, "a token-enrolled host on the tailnet; authenticating by token");
                        return self
                            .authenticate_by_token(Some(peer), hello, metadata_token)
                            .await;
                    };
                    if created.1 {
                        // A forgotten host registering afresh: the controller
                        // still excludes it until told.
                        self.controller.host_registered(&identity.node);
                        let how = match &joined {
                            Some(j) => format!(" with a join command from {}", j.created_by),
                            None => String::new(),
                        };
                        self.audit(
                            "controller",
                            "host.registered",
                            &identity.node,
                            format!(
                                "{} registered over the tailnet{how} ({:?})",
                                identity.node, created.0
                            ),
                        )
                        .await;
                    }
                    return Ok(Authenticated {
                        host: identity.node,
                        issued_token: None,
                        status: created.0,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "tailscale whois failed; trying token authentication");
                    whois_error = Some(e.description().to_string());
                }
            }
        }
        let authed = self
            .authenticate_by_token(peer, hello, metadata_token)
            .await;
        match (authed, whois_error) {
            (Err((reason, _)), Some(error)) => Err((
                format!(
                    "The server could not ask tailscaled who this agent is ({error}), so it will be retried. {reason}"
                ),
                true,
            )),
            (authed, _) => authed,
        }
    }

    /// Token authentication and bootstrap enrolment.
    async fn authenticate_by_token(
        &self,
        peer: Option<SocketAddr>,
        hello: &Hello,
        metadata_token: Option<String>,
    ) -> Result<Authenticated, (String, bool)> {
        if !self.uses(AgentAuth::Token) {
            return Err((
                "This server accepts agents over the tailnet only.".into(),
                false,
            ));
        }
        let token = if hello.token.is_empty() {
            metadata_token.unwrap_or_default()
        } else {
            hello.token.clone()
        };
        if token.is_empty() {
            return Err(("No token was presented.".into(), false));
        }
        let hash = auth::hash_token(&token);
        let h2 = hash.clone();
        let existing = self
            .store
            .call(move |db| db.hosts().by_token_hash(&h2))
            .await
            .map_err(|e| (e.description().to_string(), true))?;
        if let Some(host) = existing {
            if !hello.host_id.is_empty() && hello.host_id != host.id {
                return Err((
                    format!(
                        "This token belongs to '{}', not '{}'.",
                        host.id, hello.host_id
                    ),
                    false,
                ));
            }
            return Ok(Authenticated {
                host: host.id,
                issued_token: None,
                status: host.status,
            });
        }
        // Anything that is not a per-host token is an enrolment attempt;
        // throttle them per address so neither the bootstrap token nor a
        // join token can be guessed.
        if !self.guard.allow_enrolment(peer.map(|p| p.ip())) {
            return Err((
                "Too many enrolment attempts from this address; try again in a minute.".into(),
                true,
            ));
        }
        if !auth::valid_host_id(&hello.host_id) {
            return Err((
                "Enrolment needs a host_id of letters, digits, '.', '_' or '-'.".into(),
                false,
            ));
        }
        let joined = self.joins.redeem(&token, &hello.host_id, self.now());
        let bootstrapped = self
            .agents
            .bootstrap_token
            .as_deref()
            .is_some_and(|b| auth::constant_time_eq(&token, b));
        if joined.is_none() && !bootstrapped {
            return Err(("The token is not recognised.".into(), false));
        }
        let id = hello.host_id.clone();
        let issued = auth::new_token();
        let issued_hash = auth::hash_token(&issued);
        // A join command is the creating admin's approval.
        let status = if joined.is_some() {
            HostStatus::Active
        } else {
            self.initial_status()
        };
        let now = self.now();
        let outcome = self
            .store
            .transaction(move |db| match db.hosts().get(&id)? {
                // Never hand a token for an identity that already exists:
                // an enrolled host presents its per-host token, and a host
                // registered over the tailnet never gets one this way. A
                // forgotten host has no row and enrols afresh.
                Some(h) => Ok(Err(h.id)),
                None => {
                    db.hosts().create(&NewHost {
                        id: id.clone(),
                        status,
                        token_hash: Some(issued_hash),
                        auth_method: "token".into(),
                        at: now,
                    })?;
                    Ok(Ok(status))
                }
            })
            .await
            .map_err(|e| (e.description().to_string(), true))?;
        if let (Err(_), Some(j)) = (&outcome, &joined) {
            self.joins.release(&j.ticket);
        }
        match outcome {
            Err(id) => Err((
                format!(
                    "'{id}' is already registered; present its per-host token, or forget the host in the UI to enrol it again."
                ),
                false,
            )),
            Ok(status) => {
                // A forgotten host enrolling afresh: the controller still
                // excludes it until told.
                self.controller.host_registered(&hello.host_id);
                let how = match &joined {
                    Some(j) => format!("a join command from {}", j.created_by),
                    None => "the bootstrap token".into(),
                };
                self.audit(
                    "controller",
                    "host.enrolled",
                    &hello.host_id,
                    format!("{} enrolled with {how} ({status:?})", hello.host_id),
                )
                .await;
                Ok(Authenticated {
                    host: hello.host_id.clone(),
                    issued_token: Some(issued),
                    status,
                })
            }
        }
    }

    /// Authenticates a unary/streaming content call by per-host token or whois.
    async fn caller<T>(&self, request: &Request<T>) -> Result<String, Status> {
        if self.uses(AgentAuth::Tailscale)
            && let (Some(peer), Some(whois)) = (request.remote_addr(), &self.whois)
            && whois.covers(&peer)
            && let Ok(Some(identity)) = whois.whois(&peer).await
        {
            let trust = &self.agents.tailscale;
            if trust.is_configured() && !trust.admits(&identity.tags, identity.user.as_deref()) {
                return Err(Status::permission_denied(
                    "this tailnet node is not trusted as an agent",
                ));
            }
            let node = identity.node.clone();
            let token_enrolled = self
                .store
                .call(move |db| {
                    Ok(db
                        .hosts()
                        .get(&node)?
                        .is_some_and(|h| h.token_hash.is_some()))
                })
                .await
                .map_err(internal)?;
            if !token_enrolled {
                return Ok(identity.node);
            }
        }
        let token = request
            .metadata()
            .get(buckle_proto::TOKEN_METADATA_KEY)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| Status::unauthenticated("present the per-host token"))?;
        let hash = auth::hash_token(&token);
        self.store
            .call(move |db| db.hosts().by_token_hash(&hash))
            .await
            .map_err(internal)?
            .map(|h| h.id)
            .ok_or_else(|| Status::unauthenticated("the token is not recognised"))
    }

    /// The revisions `host` may fetch content and secrets for: only an
    /// active host, and only the stable revision and the revisions its
    /// rollouts assign it (design §06: content and secrets are scoped to what
    /// the host is meant to run).
    async fn entitled(&self, host: &str) -> Result<BTreeSet<String>, Status> {
        let h = host.to_owned();
        let (record, mut revisions, stable) = self
            .store
            .call(move |db| {
                Ok((
                    db.hosts().get(&h)?,
                    db.rollouts().revisions_for_host(&h)?,
                    db.kv().get(crate::store::kv::keys::STABLE_REVISION)?,
                ))
            })
            .await
            .map_err(internal)?;
        let record =
            record.ok_or_else(|| Status::permission_denied("the host is not registered"))?;
        if record.status != HostStatus::Active {
            return Err(Status::permission_denied("the host is not active"));
        }
        revisions.extend(stable);
        Ok(revisions)
    }

    /// Checks that `host` may fetch `revision` and that it is valid.
    async fn entitled_to(&self, host: &str, revision: &str) -> Result<(), Status> {
        if !self.entitled(host).await?.contains(revision) {
            return Err(Status::permission_denied(
                "the revision is not assigned to this host",
            ));
        }
        Ok(())
    }

    async fn welcome(&self, host: &str, authed: &Authenticated, session_id: u64) -> Welcome {
        let h = host.to_owned();
        let record = self
            .store
            .call(move |db| {
                let rec = db.hosts().get(&h)?;
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                Ok((rec, facts.unwrap_or(serde_json::Value::Null)))
            })
            .await;
        let (record, facts) = match record {
            Ok((Some(r), f)) => (r, f),
            _ => {
                return Welcome {
                    server_version: self.server_version.clone(),
                    session_id: session_id.to_string(),
                    status: status_str(authed.status).into(),
                    issued_token: authed.issued_token.clone().unwrap_or_default(),
                    ..Default::default()
                };
            }
        };
        let stable = crate::views::stable_revision(&self.store, &self.revisions).await;
        let secrets = match (&stable, record.status) {
            (Some(rev), HostStatus::Active) => {
                let ctx = crate::fleet::record_context(&record, &facts, &rev.fleet);
                Some(self.secrets.for_host(rev, &ctx).await)
            }
            _ => Some(SecretBundle::default()),
        };
        let stable_tree_digest = match &stable {
            Some(rev) => {
                let id = rev.id.0.clone();
                self.store
                    .call(move |db| Ok(db.revisions().get(&id)?.and_then(|r| r.tarball_digest)))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            }
            None => String::new(),
        };
        Welcome {
            stable_tree_digest,
            server_version: self.server_version.clone(),
            session_id: session_id.to_string(),
            stable_revision: stable.map(|r| r.id.0.clone()).unwrap_or_default(),
            issued_token: authed.issued_token.clone().unwrap_or_default(),
            labels: record.effective_labels().into_iter().collect(),
            status: status_str(record.status).into(),
            secrets,
        }
    }

    /// A durable report the store has committed (now, or on an earlier
    /// arrival when `first` is false). It is acknowledged only once the
    /// controller has saved its reaction (the pending copy is then gone): a
    /// crash in between leaves the pending copy, replayed at start-up, and
    /// the agent re-sends the unacknowledged report.
    async fn durable(&self, host: &str, id: u64, first: bool, msg: agent_message::Msg) {
        let forward = first || {
            let key = crate::rollout::pending_report_key(host, id);
            self.store
                .call(move |db| db.kv().get(&key))
                .await
                .map(|v| v.is_some())
                .unwrap_or(true)
        };
        if !forward {
            tracing::debug!(buckle.host = %host, report = id, "a report already stored and reacted to; acknowledging it again");
            self.ack(host, vec![id]);
            return;
        }
        let saved = self.controller.durable_report(host, id, msg);
        match tokio::time::timeout(DURABLE_ACK_TIMEOUT, saved).await {
            Ok(Ok(true)) => self.ack(host, vec![id]),
            Ok(_) => {
                tracing::warn!(buckle.host = %host, report = id, "the controller stopped before saving its reaction to a report; not acknowledging it");
            }
            Err(_) => {
                tracing::warn!(buckle.host = %host, report = id, "the controller has not saved its reaction to a report yet; not acknowledging it (the agent re-sends it)");
            }
        }
    }

    /// Acknowledges durable reports the store has committed.
    fn ack(&self, host: &str, report_ids: Vec<u64>) {
        self.sessions.send(
            host,
            ServerMessage {
                msg: Some(server_message::Msg::Ack(Ack { report_ids })),
            },
        );
    }

    async fn route(&self, host: &str, msg: agent_message::Msg) {
        let now = self.now();
        let h = host.to_owned();
        // Durable reports (ApplyResult, LocalRollback) carry a report id the
        // agent keeps until acknowledged; it re-sends them after every
        // Welcome, so only their first arrival has effects. `first` is
        // `Ok(true)` for every first arrival (and every other message).
        let durable_id = match &msg {
            agent_message::Msg::Result(r) if r.report_id != 0 => Some(r.report_id),
            agent_message::Msg::Rollback(lr) if lr.report_id != 0 => Some(lr.report_id),
            _ => None,
        };
        let stored: buckle_core::Result<bool> = match &msg {
            agent_message::Msg::Facts(f) => {
                let facts: serde_json::Value =
                    serde_json::from_slice(&f.json).unwrap_or(serde_json::Value::Null);
                self.store
                    .call(move |db| db.hosts().record_facts(&h, &facts, now))
                    .await
                    .map(|()| true)
            }
            agent_message::Msg::Heartbeat(hb) => {
                let applied = hb.applied_revision.clone();
                self.store
                    .call(move |db| {
                        db.hosts().touch(&h, now)?;
                        if !applied.is_empty() {
                            db.hosts().set_applied_revision(&h, &applied)?;
                        }
                        Ok(())
                    })
                    .await
                    .map(|()| true)
            }
            agent_message::Msg::Plan(p) => {
                let digest = p.digest.clone();
                self.store
                    .call(move |db| {
                        if digest.is_empty() {
                            Ok(())
                        } else {
                            db.hosts().set_last_plan_digest(&h, &digest)
                        }
                    })
                    .await
                    .map(|()| true)
            }
            agent_message::Msg::Apply(e) => {
                let event = String::from_utf8_lossy(&e.event_json).into_owned();
                let parsed: Option<buckle_core::engine::ApplyEvent> =
                    serde_json::from_str(&event).ok();
                let key = match &parsed {
                    Some(ev) => crate::views::event_key(ev),
                    None => format!("raw:{}/{}", e.module, e.step_id),
                };
                // From the wire fields, else the event itself (older agents).
                let step = parsed.as_ref().and_then(|ev| ev.step());
                let module = (!e.module.is_empty())
                    .then(|| e.module.clone())
                    .or_else(|| step.map(|(m, _)| m.to_owned()));
                let step_id = (!e.step_id.is_empty())
                    .then(|| e.step_id.clone())
                    .or_else(|| {
                        step.map(|(_, id)| id.to_owned())
                            .filter(|id| !id.is_empty())
                    });
                let state = (!e.step_state.is_empty())
                    .then(|| e.step_state.clone())
                    .or_else(|| {
                        parsed
                            .as_ref()
                            .and_then(|ev| ev.step_state())
                            .and_then(|st| serde_json::to_value(st).ok())
                            .and_then(|v| v.as_str().map(str::to_owned))
                    });
                tracing::debug!(buckle.host = %host, key = %key, event = %event, "apply event");
                let (digest, revision) = (e.plan_digest.clone(), e.revision.clone());
                let run = self
                    .store
                    .transaction(move |db| {
                        let run = open_run(db, &h, &revision, &digest, now)?;
                        db.runs().record_event(
                            &run.id,
                            &key,
                            module.as_deref(),
                            step_id.as_deref(),
                            state.as_deref(),
                            &event,
                            now,
                        )?;
                        Ok(run)
                    })
                    .await;
                run.map(|run| {
                    if let Some(event) = &parsed {
                        self.events.publish(LiveEvent::RunProgress {
                            progress: crate::views::progress(&run, event, now),
                        });
                    }
                    true
                })
            }
            agent_message::Msg::Result(r) => {
                let duration = r
                    .duration
                    .as_ref()
                    .map(|d| {
                        chrono::Duration::seconds(d.seconds)
                            + chrono::Duration::nanoseconds(d.nanos as i64)
                    })
                    .unwrap_or_default();
                let outcome: Option<buckle_core::engine::ApplyOutcome> =
                    serde_json::from_slice(&r.outcome_json).ok();
                let run = RunRecord {
                    id: String::new(),
                    host: h.clone(),
                    rollout: None,
                    revision: r.revision.clone(),
                    plan_digest: r.plan_digest.clone(),
                    started_at: now - duration,
                    finished_at: now,
                    outcome: if r.succeeded {
                        "succeeded"
                    } else if r.needs_attention {
                        "needs_attention"
                    } else {
                        "failed"
                    }
                    .into(),
                    changes: r.changes,
                    removals: r.removals,
                    error: (!r.error.is_empty()).then(|| r.error.clone()),
                    trace_id: None,
                    outcome_json: (!r.outcome_json.is_empty())
                        .then(|| String::from_utf8_lossy(&r.outcome_json).into_owned()),
                };
                let applied = (r.succeeded && !r.applied_revision.is_empty())
                    .then(|| r.applied_revision.clone());
                let pending = durable_id.map(|id| {
                    (
                        crate::rollout::pending_report_key(host, id),
                        crate::rollout::encode_pending_report(host, id, &msg),
                    )
                });
                let stored = self
                    .store
                    .transaction(move |db| {
                        if let Some(id) = durable_id
                            && !db
                                .reports()
                                .first_arrival(&run.host, id, "apply_result", now)?
                        {
                            return Ok(None);
                        }
                        // Kept until the controller has saved its reaction
                        // (replayed at start-up otherwise).
                        if let Some((key, value)) = &pending {
                            db.kv().set(key, value)?;
                        }
                        let run = finish_run(db, run)?;
                        if let Some(a) = applied {
                            db.hosts().set_applied_revision(&run.host, &a)?;
                            db.drift().clear(&run.host)?;
                        }
                        Ok(Some(run))
                    })
                    .await;
                stored.map(|run| match run {
                    Some(run) => {
                        self.events.publish(LiveEvent::RunProgress {
                            progress: crate::views::finished_progress(&run, outcome.as_ref()),
                        });
                        true
                    }
                    None => false,
                })
            }
            agent_message::Msg::Health(report) => {
                let scope = buckle_proto::v1::HealthScope::try_from(report.scope)
                    .ok()
                    .and_then(|s| s.to_core())
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".into());
                let mut events: Vec<HealthEventRecord> = report
                    .results
                    .iter()
                    .map(|r| HealthEventRecord {
                        host: h.clone(),
                        rollout: None,
                        scope: scope.clone(),
                        module: r.module.clone(),
                        check: r.check.clone(),
                        kind: r.kind.clone(),
                        required: r.required,
                        status: if r.unknown {
                            "unknown"
                        } else if r.passed {
                            "pass"
                        } else {
                            "fail"
                        }
                        .into(),
                        reason: (!r.reason.is_empty()).then(|| r.reason.clone()),
                        at: now,
                    })
                    .collect();
                let failing = report
                    .results
                    .iter()
                    .any(|r| r.required && !r.passed && !r.unknown);
                let summary = if failing {
                    HealthSummary::Failing
                } else {
                    HealthSummary::Healthy
                };
                let revision = report.revision.clone();
                self.store
                    .call(move |db| {
                        // The readings belong to the epoch of the revision
                        // they ran against: its newest run's rollout.
                        let run = if revision.is_empty() {
                            db.runs().latest_for_host(&h)?
                        } else {
                            db.runs().latest_for_revision(&h, &revision)?
                        };
                        let rollout = run.as_ref().and_then(|r| r.rollout.clone());
                        let revision = match (revision.is_empty(), run) {
                            (true, Some(r)) => r.revision,
                            _ => revision,
                        };
                        for e in &mut events {
                            e.rollout = rollout.clone();
                        }
                        db.health().record(&revision, &events)?;
                        db.hosts().set_health(&h, summary, now)
                    })
                    .await
                    .map(|()| true)
            }
            agent_message::Msg::Drift(d) => {
                let drifted =
                    !d.plan_digest.is_empty() && (d.changes + d.removals) > 0 && !d.corrected;
                let plan = d.plan_json.clone();
                self.store
                    .call(move |db| db.drift().record(&h, drifted, &plan, now))
                    .await
                    .map(|()| true)
            }
            agent_message::Msg::Rollback(lr) => {
                let to = lr.to_revision.clone();
                let kind = if lr.modules.is_empty() {
                    "local_rollback"
                } else {
                    "module_rollback"
                };
                let pending = durable_id.map(|id| {
                    (
                        crate::rollout::pending_report_key(host, id),
                        crate::rollout::encode_pending_report(host, id, &msg),
                    )
                });
                let first = self
                    .store
                    .transaction(move |db| {
                        if let Some(id) = durable_id
                            && !db.reports().first_arrival(&h, id, kind, now)?
                        {
                            return Ok(false);
                        }
                        if let Some((key, value)) = &pending {
                            db.kv().set(key, value)?;
                        }
                        // A module rollback leaves the host on the revision
                        // it was applying (that module back on stable).
                        if !to.is_empty() && kind == "local_rollback" {
                            db.hosts().set_applied_revision(&h, &to)?;
                        }
                        Ok(true)
                    })
                    .await;
                if matches!(first, Ok(true)) {
                    self.audit(
                        host,
                        &format!("host.{kind}"),
                        host,
                        rollback_message(host, lr),
                    )
                    .await;
                }
                first
            }
            agent_message::Msg::Upgrade(u) => {
                let outcome =
                    buckle_proto::v1::UpgradeOutcome::try_from(u.outcome).unwrap_or_default();
                if let Some(settled) = self.upgrades.reported(host, &u.version, outcome, &u.error) {
                    self.settled(host, settled).await;
                }
                Ok(true)
            }
            agent_message::Msg::RebootFailed(f) => {
                self.audit(
                    host,
                    "host.reboot_failed",
                    host,
                    format!(
                        "{host}: the reboot its run asked for did not happen: {}",
                        f.error
                    ),
                )
                .await;
                Ok(true)
            }
            agent_message::Msg::Hello(_) => Ok(true),
        };
        match &stored {
            Err(e) => {
                tracing::warn!(buckle.host = %host, error = %e, "an agent report could not be stored");
            }
            Ok(first) => {
                if let Some(id) = durable_id {
                    self.durable(host, id, *first, msg.clone()).await;
                    if *first {
                        self.publish_host(host).await;
                    }
                    return;
                }
            }
        }
        // A drift pass with an empty digest found nothing; only a real
        // drift is worth telling the UI about.
        let notify_ui = match &msg {
            agent_message::Msg::Drift(d) => !d.plan_digest.is_empty(),
            agent_message::Msg::Health(_)
            | agent_message::Msg::Result(_)
            | agent_message::Msg::Rollback(_)
            | agent_message::Msg::Upgrade(_)
            | agent_message::Msg::RebootFailed(_) => true,
            _ => false,
        };
        if matches!(
            msg,
            agent_message::Msg::Plan(_)
                | agent_message::Msg::Result(_)
                | agent_message::Msg::Health(_)
                | agent_message::Msg::Rollback(_)
                | agent_message::Msg::RebootFailed(_)
        ) {
            self.controller.agent_message(host, msg);
        }
        if notify_ui {
            self.publish_host(host).await;
        }
    }
}

/// The host's open run for the plan `digest`, started now when there is none
/// (a run of another plan still open is abandoned: its result never came).
fn open_run(
    db: &crate::store::Db<'_>,
    host: &str,
    revision: &str,
    digest: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> buckle_core::Result<RunRecord> {
    if let Some(mut open) = db.runs().open_for_host(host)? {
        if open.plan_digest == digest {
            // A run of a rollout always names it; look again in case the
            // rollout's row was not there at the first event.
            if open.rollout.is_none()
                && let Some(rollout) = db.rollouts().applying_with_plan(host, digest)?
            {
                db.runs().set_rollout(&open.id, &rollout)?;
                open.rollout = Some(rollout);
            }
            return Ok(open);
        }
        db.runs().abandon(&open.id, now)?;
    }
    let mut run = RunRecord {
        id: RunRecord::new_id(),
        host: host.to_owned(),
        rollout: db.rollouts().applying_with_plan(host, digest)?,
        revision: revision.to_owned(),
        plan_digest: digest.to_owned(),
        started_at: now,
        finished_at: now,
        outcome: crate::store::RUNNING.to_owned(),
        changes: 0,
        removals: 0,
        error: None,
        trace_id: None,
        outcome_json: None,
    };
    run.id = db.runs().add(&run)?;
    Ok(run)
}

/// Completes the run `result` reports on (the open one, or one abandoned
/// before its result arrived), or records it when there is none. Returns
/// the stored run.
fn finish_run(db: &crate::store::Db<'_>, mut result: RunRecord) -> buckle_core::Result<RunRecord> {
    // A run of another plan still open will get no result of its own.
    if let Some(open) = db.runs().open_for_host(&result.host)?
        && open.plan_digest != result.plan_digest
    {
        db.runs().abandon(&open.id, result.finished_at)?;
    }
    match db.runs().unresolved(&result.host, &result.plan_digest)? {
        Some(open) => {
            result.id = open.id;
            result.started_at = open.started_at;
            result.rollout = match open.rollout {
                Some(rollout) => Some(rollout),
                None => db
                    .rollouts()
                    .applying_with_plan(&result.host, &result.plan_digest)?,
            };
            if let Some(rollout) = &result.rollout {
                db.runs().set_rollout(&result.id, rollout)?;
            }
            db.runs().finish(&result)?;
        }
        None => {
            result.rollout = db
                .rollouts()
                .applying_with_plan(&result.host, &result.plan_digest)?;
            result.id = db.runs().add(&result)?;
        }
    }
    Ok(result)
}

/// The audit message for a local or module rollback, with the rollback's own
/// result when it failed (the host then needs attention).
fn rollback_message(host: &str, lr: &buckle_proto::v1::LocalRollback) -> String {
    let what = if lr.modules.is_empty() {
        format!(
            "{host} rolled itself back from {} to {}",
            lr.from_revision, lr.to_revision
        )
    } else {
        format!(
            "{host} rolled module(s) {} back to {}",
            lr.modules.join(", "),
            lr.to_revision
        )
    };
    if lr.succeeded {
        format!("{what}: {}", lr.reason)
    } else {
        format!(
            "{what}: {}; the rollback itself failed ({}); the host needs attention",
            lr.reason,
            if lr.rollback_error.is_empty() {
                "no detail"
            } else {
                lr.rollback_error.as_str()
            }
        )
    }
}

fn status_str(s: HostStatus) -> &'static str {
    match s {
        HostStatus::Pending => "pending",
        HostStatus::Active => "active",
        HostStatus::Paused => "paused",
    }
}

async fn file_chunks(path: std::path::PathBuf, digest: String) -> Result<ChunkStream, Status> {
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| Status::not_found("the content is not available on this server"))?;
    let total = bytes.len() as u64;
    let mut chunks: Vec<Chunk> = bytes
        .chunks(CHUNK)
        .map(|c| Chunk {
            data: c.to_vec(),
            total: 0,
            digest: String::new(),
        })
        .collect();
    if chunks.is_empty() {
        chunks.push(Chunk::default());
    }
    chunks[0].total = total;
    if let Some(last) = chunks.last_mut() {
        last.digest = digest;
    }
    Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
}

#[tonic::async_trait]
impl Fleet for FleetService {
    type SessionStream = ServerStream;
    type FetchRevisionStream = ChunkStream;
    type FetchBlobStream = ChunkStream;

    async fn session(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let peer = request.remote_addr();
        let metadata_token = request
            .metadata()
            .get(buckle_proto::TOKEN_METADATA_KEY)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut inbound = request.into_inner();
        let first = match tokio::time::timeout(self.guard.hello_timeout, inbound.message()).await {
            Ok(m) => m?,
            Err(_) => {
                tracing::debug!(peer = ?peer, "a session sent no Hello in time");
                return Ok(Response::new(rejected(
                    "The session must start with Hello.",
                    true,
                )));
            }
        };
        let hello = match first {
            Some(AgentMessage {
                msg: Some(agent_message::Msg::Hello(h)),
            }) => h,
            _ => {
                return Ok(Response::new(rejected(
                    "The session must start with Hello.",
                    false,
                )));
            }
        };
        // Before anything is written: a refused old agent must be able to
        // enrol once it has been upgraded.
        if let Some(min) = &self.agents.minimum_agent_version
            && !auth::version_at_least(&hello.agent_version, min)
        {
            return Ok(Response::new(rejected(
                format!(
                    "Agent version {} is older than this server's minimum {min}; upgrade the agent.",
                    hello.agent_version
                ),
                false,
            )));
        }
        let authed = match self.authenticate(peer, &hello, metadata_token).await {
            Ok(a) => a,
            Err((reason, retry)) => {
                tracing::warn!(peer = ?peer, host = %hello.host_id, reason = %reason, "agent rejected");
                return Ok(Response::new(rejected(reason, retry)));
            }
        };
        let host = authed.host.clone();
        let now = self.now();
        let abandoned = {
            let (h, hello2) = (host.clone(), hello.clone());
            let labels: BTreeMap<String, String> = hello.labels.clone().into_iter().collect();
            self.store
                .call(move |db| {
                    db.hosts().record_hello(
                        &h,
                        &hello2.agent_version,
                        &hello2.platform,
                        &hello2.arch,
                        &labels,
                        Some(hello2.applied_revision.as_str()),
                        Some(hello2.last_plan_digest.as_str()),
                        now,
                    )?;
                    // A run the new session does not say is still running
                    // (or reporting) will get no more events or result.
                    db.runs()
                        .abandon_unless_unreported(&h, &hello2.unreported_apply_revisions, now)
                })
                .await
                .map_err(internal)?
        };
        if let Some(run) = abandoned {
            tracing::info!(buckle.host = %host, run = %run.id, "the agent reconnected without the run's apply; the run is closed as abandoned");
            self.events.publish(LiveEvent::RunProgress {
                progress: crate::views::finished_progress(&run, None),
            });
        }
        let (session_id, rx) = self.sessions.attach(&host);
        let closed = self.guard.sessions.register(&host, session_id);
        let welcome = self.welcome(&host, &authed, session_id).await;
        // Queued behind the Welcome: a re-sent Upgrade reaches the new session.
        if let Some(settled) = self.upgrades.connected(&host, &hello.agent_version) {
            self.settled(&host, settled).await;
        }
        tracing::info!(buckle.host = %host, session = session_id, status = %welcome.status, version = %hello.agent_version, "agent connected");
        self.controller.connected(Connected {
            host: host.clone(),
            applied_revision: (!hello.applied_revision.is_empty())
                .then(|| hello.applied_revision.clone()),
            last_plan_digest: (!hello.last_plan_digest.is_empty())
                .then(|| hello.last_plan_digest.clone()),
            unreported_apply_revisions: hello.unreported_apply_revisions.clone(),
            new_session: true,
            capabilities: hello.capabilities.clone(),
            boot_id: (!hello.boot_id.is_empty()).then(|| hello.boot_id.clone()),
        });
        self.publish_host(&host).await;

        let svc = self.clone();
        let reader_host = host.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    _ = reader_closed.cancelled() => {
                        tracing::info!(buckle.host = %reader_host, session = session_id, "session closed by the server");
                        break;
                    }
                    m = inbound.message() => m,
                };
                match next {
                    Ok(Some(AgentMessage { msg: Some(m) })) => svc.route(&reader_host, m).await,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(buckle.host = %reader_host, error = %e, "session stream ended with an error");
                        break;
                    }
                }
            }
            svc.guard.sessions.release(&reader_host, session_id);
            if svc.sessions.detach(&reader_host, session_id) {
                let now = svc.now();
                let h = reader_host.clone();
                let _ = svc
                    .store
                    .call(move |db| db.hosts().set_connection(&h, LinkState::Offline, now))
                    .await;
                svc.controller.disconnected(&reader_host);
                svc.publish_host(&reader_host).await;
                tracing::info!(buckle.host = %reader_host, session = session_id, "agent disconnected");
            }
        });

        let first = ServerMessage {
            msg: Some(server_message::Msg::Welcome(welcome)),
        };
        let stream = futures::stream::once(async move { Ok(first) })
            .chain(UnboundedReceiverStream::new(rx).map(Ok))
            .take_until(closed.cancelled_owned());
        Ok(Response::new(Box::pin(stream)))
    }

    async fn fetch_revision(
        &self,
        request: Request<FetchRevisionRequest>,
    ) -> Result<Response<Self::FetchRevisionStream>, Status> {
        let host = self.caller(&request).await?;
        let id = request.into_inner().revision;
        self.entitled_to(&host, &id).await?;
        let id2 = id.clone();
        let record = self
            .store
            .call(move |db| db.revisions().get(&id2))
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("no such revision"))?;
        let Some(digest) = record.tarball_digest else {
            return Err(Status::failed_precondition(
                "the revision was rejected and has no content",
            ));
        };
        Ok(Response::new(
            file_chunks(self.revisions.tarball(&id), digest).await?,
        ))
    }

    async fn fetch_blob(
        &self,
        request: Request<FetchBlobRequest>,
    ) -> Result<Response<Self::FetchBlobStream>, Status> {
        let host = self.caller(&request).await?;
        let digest = request.into_inner().digest;
        let allowed: Vec<String> = self.entitled(&host).await?.into_iter().collect();
        let d2 = digest.clone();
        let (revision, path) = self
            .store
            .call(move |db| db.revisions().find_blob_in(&d2, &allowed))
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("no such blob"))?;
        let full = self.revisions.dir(&revision).join(&path);
        Ok(Response::new(file_chunks(full, digest).await?))
    }

    async fn fetch_secrets(
        &self,
        request: Request<FetchSecretsRequest>,
    ) -> Result<Response<SecretBundle>, Status> {
        let host = self.caller(&request).await?;
        let req = request.into_inner();
        self.entitled_to(&host, &req.revision).await?;
        let (h, r) = (host.clone(), req.revision.clone());
        let (record, facts, revision_record) = self
            .store
            .call(move |db| {
                let rec = db.hosts().get(&h)?;
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                Ok((
                    rec,
                    facts.unwrap_or(serde_json::Value::Null),
                    db.revisions().get(&r)?,
                ))
            })
            .await
            .map_err(internal)?;
        let record = record.ok_or_else(|| Status::unauthenticated("unknown host"))?;
        if record.status != HostStatus::Active {
            return Err(Status::permission_denied("the host is not active"));
        }
        if revision_record.map(|r| r.status) != Some(RevisionStatus::Valid) {
            return Err(Status::failed_precondition("the revision is not valid"));
        }
        let revision = self
            .revisions
            .load(&req.revision)
            .await
            .map_err(|_| Status::not_found("no such revision"))?;
        let ctx = crate::fleet::record_context(&record, &facts, &revision.fleet);
        let bundle = self.secrets.fetch(&revision, &ctx, &req.names).await;
        tracing::info!(buckle.host = %host, requested = req.names.len(), granted = bundle.secrets.len(), "secrets fetched");
        Ok(Response::new(bundle))
    }
}
