//! An in-process `Fleet` server implemented with tonic. Tests accept sessions
//! one at a time, script ServerMessages into them and read the AgentMessages
//! the agent sends back.

use buckle_proto::v1 as pb;
use buckle_proto::v1::fleet_server::{Fleet, FleetServer};
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tonic::{Request, Response, Status, Streaming};

/// How long test helpers wait (in tokio time, which may be paused).
pub const WAIT: Duration = Duration::from_secs(3600);

/// A session the agent opened.
pub struct ServerSession {
    /// The `x-buckle-token` metadata of the call.
    pub token: Option<String>,
    /// The Hello the agent sent first.
    pub hello: pb::Hello,
    tx: Option<mpsc::Sender<Result<pb::ServerMessage, Status>>>,
    rx: mpsc::UnboundedReceiver<(Instant, pb::AgentMessage)>,
}

/// An AgentMessage with the (tokio) time it arrived.
pub type Received = (Instant, pb::agent_message::Msg);

impl ServerSession {
    /// Sends a message to the agent.
    pub async fn send(&self, msg: pb::server_message::Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Ok(pb::ServerMessage { msg: Some(msg) })).await;
        }
    }

    /// Ends the session from the server side.
    pub fn close(&mut self) {
        self.tx = None;
    }

    /// The next message, or `None` when the agent closed the stream.
    pub async fn recv(&mut self) -> Option<Received> {
        let (at, message) = tokio::time::timeout(WAIT, self.rx.recv())
            .await
            .expect("timed out waiting for the agent")?;
        Some((at, message.msg.expect("message has a body")))
    }

    /// Skips messages until `f` maps one to `Some`.
    pub async fn expect<T>(&mut self, f: impl Fn(&pb::agent_message::Msg) -> Option<T>) -> T {
        loop {
            let (_, msg) = self.recv().await.expect("the agent closed the session");
            if let Some(t) = f(&msg) {
                return t;
            }
        }
    }

    /// Collects every message until `stop` matches one (included).
    pub async fn collect_until(
        &mut self,
        stop: impl Fn(&pb::agent_message::Msg) -> bool,
    ) -> Vec<Received> {
        let mut out = Vec::new();
        loop {
            let received = self.recv().await.expect("the agent closed the session");
            let done = stop(&received.1);
            out.push(received);
            if done {
                return out;
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    sessions: Mutex<Option<mpsc::UnboundedSender<ServerSession>>>,
    revisions: Mutex<HashMap<String, Vec<u8>>>,
    secrets: Mutex<Vec<pb::SecretValue>>,
    corrupt: AtomicBool,
    unreachable: AtomicBool,
    attempts: Mutex<Vec<Instant>>,
}

/// The fake server.
#[derive(Clone, Default)]
pub struct FakeFleet {
    inner: Arc<Inner>,
}

/// Receives the sessions agents open.
pub struct Sessions {
    rx: mpsc::UnboundedReceiver<ServerSession>,
}

impl Sessions {
    /// The next session.
    pub async fn accept(&mut self) -> ServerSession {
        tokio::time::timeout(WAIT, self.rx.recv())
            .await
            .expect("timed out waiting for a session")
            .expect("server dropped")
    }
}

impl FakeFleet {
    /// A server and the receiver of its sessions.
    pub fn new() -> (Self, Sessions) {
        let fleet = FakeFleet::default();
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut g) = fleet.inner.sessions.lock() {
            *g = Some(tx);
        }
        (fleet, Sessions { rx })
    }

    /// Serves `bundle` (a tar.gz) as revision `id`; returns its digest.
    pub fn add_revision(&self, id: &str, bundle: Vec<u8>) -> String {
        let digest = format!("sha256:{}", crate::bundle::sha256_hex(&bundle));
        self.inner
            .revisions
            .lock()
            .expect("lock")
            .insert(id.to_owned(), bundle);
        digest
    }

    /// Secrets returned by FetchSecrets.
    pub fn set_secrets(&self, secrets: Vec<pb::SecretValue>) {
        *self.inner.secrets.lock().expect("lock") = secrets;
    }

    /// Flip a byte of every revision sent (keeping the true digest).
    pub fn set_corrupt(&self, corrupt: bool) {
        self.inner.corrupt.store(corrupt, Ordering::SeqCst);
    }

    /// Refuse sessions with UNAVAILABLE.
    pub fn set_unreachable(&self, unreachable: bool) {
        self.inner.unreachable.store(unreachable, Ordering::SeqCst);
    }

    /// When the agent tried to open a session (accepted or not).
    pub fn attempts(&self) -> Vec<Instant> {
        self.inner.attempts.lock().expect("lock").clone()
    }

    /// The tonic service.
    pub fn service(&self) -> FleetServer<FakeFleet> {
        FleetServer::new(self.clone())
    }

    /// Serves over in-memory pipes; the returned channel connects to it.
    pub fn in_memory_channel(&self) -> tonic::transport::Channel {
        let (conn_tx, conn_rx) =
            mpsc::unbounded_channel::<Result<tokio::io::DuplexStream, std::io::Error>>();
        let service = self.service();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(UnboundedReceiverStream::new(conn_rx))
                .await;
        });
        tonic::transport::Endpoint::from_static("http://fake.buckle").connect_with_connector_lazy(
            tower::service_fn(move |_: tonic::transport::Uri| {
                let conn_tx = conn_tx.clone();
                async move {
                    let (client, server) = tokio::io::duplex(1 << 20);
                    conn_tx
                        .send(Ok(server))
                        .map_err(|_| std::io::Error::other("server stopped"))?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(client))
                }
            }),
        )
    }

    /// Serves on a random localhost TCP port; returns the URL.
    pub async fn serve_tcp(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let service = self.service();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        format!("http://{addr}")
    }
}

type ServerStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl Fleet for FakeFleet {
    type SessionStream = ServerStream<pb::ServerMessage>;
    type FetchRevisionStream = ServerStream<pb::Chunk>;
    type FetchBlobStream = ServerStream<pb::Chunk>;

    async fn session(
        &self,
        request: Request<Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        self.inner
            .attempts
            .lock()
            .expect("lock")
            .push(Instant::now());
        if self.inner.unreachable.load(Ordering::SeqCst) {
            return Err(Status::unavailable("the fake server is unreachable"));
        }
        let token = request
            .metadata()
            .get(buckle_proto::TOKEN_METADATA_KEY)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut inbound = request.into_inner();
        let hello = match inbound.message().await? {
            Some(pb::AgentMessage {
                msg: Some(pb::agent_message::Msg::Hello(hello)),
            }) => hello,
            other => {
                return Err(Status::invalid_argument(format!(
                    "expected Hello first, got {other:?}"
                )));
            }
        };
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(message)) = inbound.message().await {
                if in_tx.send((Instant::now(), message)).is_err() {
                    break;
                }
            }
        });
        let (out_tx, out_rx) = mpsc::channel(64);
        let session = ServerSession {
            token,
            hello,
            tx: Some(out_tx),
            rx: in_rx,
        };
        if let Some(sessions) = self.inner.sessions.lock().expect("lock").as_ref() {
            let _ = sessions.send(session);
        }
        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn fetch_revision(
        &self,
        request: Request<pb::FetchRevisionRequest>,
    ) -> Result<Response<Self::FetchRevisionStream>, Status> {
        let id = request.into_inner().revision;
        let bundle = self
            .inner
            .revisions
            .lock()
            .expect("lock")
            .get(&id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no revision {id}")))?;
        let digest = format!("sha256:{}", crate::bundle::sha256_hex(&bundle));
        let mut data = bundle.clone();
        if self.inner.corrupt.load(Ordering::SeqCst) {
            let mid = data.len() / 2;
            data[mid] ^= 0xff;
        }
        let pieces: Vec<Vec<u8>> = data.chunks(512).map(<[u8]>::to_vec).collect();
        let n = pieces.len();
        let chunks: Vec<Result<pb::Chunk, Status>> = pieces
            .into_iter()
            .enumerate()
            .map(|(i, piece)| {
                Ok(pb::Chunk {
                    data: piece,
                    total: if i == 0 { bundle.len() as u64 } else { 0 },
                    digest: if i + 1 == n {
                        digest.clone()
                    } else {
                        String::new()
                    },
                })
            })
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(chunks))))
    }

    async fn fetch_blob(
        &self,
        _request: Request<pb::FetchBlobRequest>,
    ) -> Result<Response<Self::FetchBlobStream>, Status> {
        Err(Status::unimplemented("no blobs in the fake server"))
    }

    async fn fetch_secrets(
        &self,
        request: Request<pb::FetchSecretsRequest>,
    ) -> Result<Response<pb::SecretBundle>, Status> {
        let names = request.into_inner().names;
        let secrets = self
            .inner
            .secrets
            .lock()
            .expect("lock")
            .iter()
            .filter(|s| names.contains(&s.name))
            .cloned()
            .collect();
        Ok(Response::new(pb::SecretBundle { secrets }))
    }

    /// Echoes the request in the token; refuses the target `denied`.
    async fn issue_identity(
        &self,
        request: Request<pb::IssueIdentityRequest>,
    ) -> Result<Response<pb::IssuedIdentity>, Status> {
        let r = request.into_inner();
        if r.target == "denied" {
            return Err(Status::permission_denied(
                "test-host is not an executor for denied",
            ));
        }
        Ok(Response::new(pb::IssuedIdentity {
            token: format!(
                "{}/{:?}/{}/{}",
                r.target,
                r.purpose(),
                r.revision,
                r.rollout_id
            ),
            expires_at: Some(prost_types::Timestamp {
                seconds: 1_893_456_000,
                nanos: 0,
            }),
            issuer: "buckle".into(),
            audience: r.target,
            id: "jti-1".into(),
        }))
    }
}
