//! The `Fleet` service.

use super::auth::{self, Whois};
use super::tls::ServerIdentity;
use crate::clock::Clock;
use crate::config::{AgentAuth, AgentsConfig, Registration};
use crate::events::EventBus;
use crate::rollout::{Connected, ControllerHandle, Sessions};
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{HealthEventRecord, NewAudit, NewHost, RunRecord, Store};
use buckle_api::{Connection as LinkState, HealthSummary, HostStatus, LiveEvent};
use buckle_proto::v1::fleet_server::{Fleet, FleetServer};
use buckle_proto::v1::{
    AgentMessage, Chunk, FetchBlobRequest, FetchRevisionRequest, FetchSecretsRequest, Hello,
    Rejected, SecretBundle, ServerMessage, Welcome, agent_message, server_message,
};
use futures::{Stream, StreamExt};
use std::collections::BTreeMap;
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
}

impl std::fmt::Debug for FleetService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetService").finish_non_exhaustive()
    }
}

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
        .add_service(FleetServer::new(service))
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
        if let Some(host) = crate::views::load_summary(&self.store, &self.revisions, id).await {
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
                    let id = identity.node.clone();
                    let status = self.initial_status();
                    let now = self.now();
                    let created = self
                        .store
                        .call(move |db| match db.hosts().get(&id)? {
                            Some(h) => Ok((h.status, false)),
                            None => {
                                db.hosts().create(&NewHost {
                                    id: id.clone(),
                                    status,
                                    token_hash: None,
                                    auth_method: "tailscale".into(),
                                    at: now,
                                })?;
                                Ok((status, true))
                            }
                        })
                        .await
                        .map_err(|e| (e.description().to_string(), true))?;
                    if created.1 {
                        self.audit(
                            "controller",
                            "host.registered",
                            &identity.node,
                            format!(
                                "{} registered over the tailnet ({:?})",
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
                    tracing::warn!(error = %e, "tailscale whois failed; trying token authentication")
                }
            }
        }
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
        let Some(bootstrap) = &self.agents.bootstrap_token else {
            return Err(("The token is not recognised.".into(), false));
        };
        if !auth::constant_time_eq(&token, bootstrap) {
            return Err(("The token is not recognised.".into(), false));
        }
        if !auth::valid_host_id(&hello.host_id) {
            return Err((
                "Enrolment needs a host_id of letters, digits, '.', '_' or '-'.".into(),
                false,
            ));
        }
        let id = hello.host_id.clone();
        let issued = auth::new_token();
        let issued_hash = auth::hash_token(&issued);
        let status = self.initial_status();
        let now = self.now();
        let outcome = self
            .store
            .transaction(move |db| match db.hosts().get(&id)? {
                Some(h) if h.token_hash.is_some() => Ok(Err(h.id)),
                Some(h) => {
                    db.hosts().set_token_hash(&h.id, &issued_hash)?;
                    Ok(Ok(h.status))
                }
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
        match outcome {
            Err(id) => Err((
                format!(
                    "'{id}' is already enrolled; present its per-host token instead of the bootstrap token."
                ),
                false,
            )),
            Ok(status) => {
                self.audit(
                    "controller",
                    "host.enrolled",
                    &hello.host_id,
                    format!(
                        "{} enrolled with the bootstrap token ({status:?})",
                        hello.host_id
                    ),
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
            return Ok(identity.node);
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
        Welcome {
            server_version: self.server_version.clone(),
            session_id: session_id.to_string(),
            stable_revision: stable.map(|r| r.id.0.clone()).unwrap_or_default(),
            issued_token: authed.issued_token.clone().unwrap_or_default(),
            labels: record.effective_labels().into_iter().collect(),
            status: status_str(record.status).into(),
            secrets,
        }
    }

    async fn route(&self, host: &str, msg: agent_message::Msg) {
        let now = self.now();
        let h = host.to_owned();
        let stored = match &msg {
            agent_message::Msg::Facts(f) => {
                let facts: serde_json::Value =
                    serde_json::from_slice(&f.json).unwrap_or(serde_json::Value::Null);
                self.store
                    .call(move |db| db.hosts().record_facts(&h, &facts, now))
                    .await
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
            }
            agent_message::Msg::Apply(e) => {
                let event = String::from_utf8_lossy(&e.event_json).into_owned();
                let parsed: serde_json::Value = serde_json::from_str(&event).unwrap_or_default();
                let step = format!(
                    "{}/{}",
                    parsed["module"].as_str().unwrap_or(""),
                    parsed["task_id"]
                        .as_str()
                        .or(parsed["check"].as_str())
                        .unwrap_or(parsed["event"].as_str().unwrap_or(""))
                );
                tracing::debug!(buckle.host = %host, step = %step, event = %event, "apply event");
                let digest = e.plan_digest.clone();
                self.store
                    .call(move |db| db.runs().record_event(&h, &digest, &step, &event, now))
                    .await
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
                let run = RunRecord {
                    host: h.clone(),
                    rollout: None,
                    revision: r.revision.clone(),
                    plan_digest: r.plan_digest.clone(),
                    started_at: now - duration,
                    finished_at: now,
                    outcome: if r.stale_plan {
                        "stale_plan"
                    } else if r.succeeded {
                        "succeeded"
                    } else {
                        "failed"
                    }
                    .into(),
                    changes: r.changes,
                    removals: r.removals,
                    error: (!r.error.is_empty()).then(|| r.error.clone()),
                    trace_id: None,
                };
                let applied = (r.succeeded && !r.applied_revision.is_empty())
                    .then(|| r.applied_revision.clone());
                self.store
                    .call(move |db| {
                        db.runs().add(&run)?;
                        if let Some(a) = applied {
                            db.hosts().set_applied_revision(&run.host, &a)?;
                        }
                        Ok(())
                    })
                    .await
            }
            agent_message::Msg::Health(report) => {
                let scope = buckle_proto::v1::HealthScope::try_from(report.scope)
                    .ok()
                    .and_then(|s| s.to_core())
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".into());
                let events: Vec<HealthEventRecord> = report
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
                self.store
                    .call(move |db| {
                        db.health().record(&events)?;
                        db.hosts().set_health(&h, summary, now)
                    })
                    .await
            }
            agent_message::Msg::Drift(d) => {
                let drifted =
                    !d.plan_digest.is_empty() && (d.changes + d.removals) > 0 && !d.corrected;
                self.store
                    .call(move |db| db.hosts().set_drifted(&h, drifted))
                    .await
            }
            agent_message::Msg::Rollback(lr) => {
                let to = lr.to_revision.clone();
                let r = self
                    .store
                    .call(move |db| {
                        if to.is_empty() {
                            Ok(())
                        } else {
                            db.hosts().set_applied_revision(&h, &to)
                        }
                    })
                    .await;
                self.audit(
                    host,
                    "host.local_rollback",
                    host,
                    format!(
                        "{host} rolled itself back from {} to {}: {}",
                        lr.from_revision, lr.to_revision, lr.reason
                    ),
                )
                .await;
                r
            }
            agent_message::Msg::Hello(_) => Ok(()),
        };
        if let Err(e) = stored {
            tracing::warn!(buckle.host = %host, error = %e, "an agent report could not be stored");
        }
        let notify_ui = matches!(
            msg,
            agent_message::Msg::Health(_)
                | agent_message::Msg::Drift(_)
                | agent_message::Msg::Result(_)
                | agent_message::Msg::Rollback(_)
        );
        if matches!(
            msg,
            agent_message::Msg::Plan(_)
                | agent_message::Msg::Result(_)
                | agent_message::Msg::Health(_)
                | agent_message::Msg::Rollback(_)
        ) {
            self.controller.agent_message(host, msg);
        }
        if notify_ui {
            self.publish_host(host).await;
        }
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
        let hello = match inbound.message().await? {
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
        let authed = match self.authenticate(peer, &hello, metadata_token).await {
            Ok(a) => a,
            Err((reason, retry)) => {
                tracing::warn!(peer = ?peer, host = %hello.host_id, reason = %reason, "agent rejected");
                return Ok(Response::new(rejected(reason, retry)));
            }
        };
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
        let host = authed.host.clone();
        let now = self.now();
        {
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
                    )
                })
                .await
                .map_err(internal)?;
        }
        let (session_id, rx) = self.sessions.attach(&host);
        let welcome = self.welcome(&host, &authed, session_id).await;
        tracing::info!(buckle.host = %host, session = session_id, status = %welcome.status, version = %hello.agent_version, "agent connected");
        self.controller.connected(Connected {
            host: host.clone(),
            applied_revision: (!hello.applied_revision.is_empty())
                .then(|| hello.applied_revision.clone()),
            last_plan_digest: (!hello.last_plan_digest.is_empty())
                .then(|| hello.last_plan_digest.clone()),
        });
        self.publish_host(&host).await;

        let svc = self.clone();
        let reader_host = host.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(AgentMessage { msg: Some(m) })) => svc.route(&reader_host, m).await,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(buckle.host = %reader_host, error = %e, "session stream ended with an error");
                        break;
                    }
                }
            }
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
            .chain(UnboundedReceiverStream::new(rx).map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn fetch_revision(
        &self,
        request: Request<FetchRevisionRequest>,
    ) -> Result<Response<Self::FetchRevisionStream>, Status> {
        self.caller(&request).await?;
        let id = request.into_inner().revision;
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
        self.caller(&request).await?;
        let digest = request.into_inner().digest;
        let d2 = digest.clone();
        let (revision, path) = self
            .store
            .call(move |db| db.revisions().find_blob(&d2))
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
        let h = host.clone();
        let (record, facts) = self
            .store
            .call(move |db| {
                let rec = db.hosts().get(&h)?;
                let facts = db.hosts().latest_facts(&h)?.map(|(f, _)| f);
                Ok((rec, facts.unwrap_or(serde_json::Value::Null)))
            })
            .await
            .map_err(internal)?;
        let record = record.ok_or_else(|| Status::unauthenticated("unknown host"))?;
        if record.status != HostStatus::Active {
            return Err(Status::permission_denied("the host is not active"));
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
