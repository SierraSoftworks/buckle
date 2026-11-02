//! The gRPC front end to end with an in-process tonic client.

use super::*;
use crate::clock::TokioClock;
use crate::config::{AgentAuth, AgentsConfig, Registration};
use crate::events::EventBus;
use crate::rollout::{ControllerDeps, ControllerOptions, Sessions};
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::source::{CommitInfo, RevisionBuilder};
use crate::store::Store;
use async_trait::async_trait;
use buckle_core::secrets::InstallKey;
use buckle_proto::v1::fleet_client::FleetClient;
use buckle_proto::v1::{
    AgentMessage, Facts, FetchRevisionRequest, Hello, ServerMessage, agent_message, server_message,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

struct FakeWhois;

#[async_trait]
impl Whois for FakeWhois {
    fn covers(&self, _peer: &SocketAddr) -> bool {
        true
    }
    async fn whois(&self, _peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        Ok(Some(WhoisIdentity {
            node: "tail-1".into(),
            user: None,
        }))
    }
}

struct Server {
    addr: SocketAddr,
    store: Store,
    data: tempfile::TempDir,
    revisions: Arc<RevisionCache>,
    cancel: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn server(
    registration: Registration,
    whois: Option<Arc<dyn Whois>>,
    auth: Vec<AgentAuth>,
) -> Server {
    let data = tempfile::tempdir().unwrap();
    let store = Store::in_memory().await.unwrap();
    let sessions = Arc::new(Sessions::new());
    let revisions = Arc::new(RevisionCache::new(data.path()));
    let clock = Arc::new(TokioClock::new());
    let secrets = ServerSecrets::new(&Default::default(), InstallKey::from_bytes([1; 32]));
    let events = EventBus::default();
    let cancel = CancellationToken::new();
    let (controller, _task) = crate::rollout::spawn(
        ControllerDeps {
            store: store.clone(),
            link: sessions.clone(),
            clock: clock.clone(),
            events: events.clone(),
            revisions: revisions.clone(),
            secrets: secrets.clone(),
            approval_key: [1; 32],
            options: ControllerOptions::default(),
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    let service = FleetService {
        store: store.clone(),
        sessions,
        controller,
        revisions: revisions.clone(),
        secrets,
        agents: AgentsConfig {
            auth,
            bootstrap_token: Some("boot".into()),
            registration,
            minimum_agent_version: Some("2.0.0".into()),
            ..Default::default()
        },
        whois,
        events,
        clock,
        server_version: "2.0.0-test".into(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(service, listener, None, cancel.clone()));
    Server {
        addr,
        store,
        data,
        revisions,
        cancel,
    }
}

fn hello(host: &str, token: &str, version: &str) -> AgentMessage {
    AgentMessage {
        msg: Some(agent_message::Msg::Hello(Hello {
            host_id: host.into(),
            agent_version: version.into(),
            token: token.into(),
            platform: "linux".into(),
            arch: "aarch64".into(),
            ..Default::default()
        })),
    }
}

async fn open(
    addr: SocketAddr,
    first: AgentMessage,
) -> (
    tokio::sync::mpsc::Sender<AgentMessage>,
    tonic::Streaming<ServerMessage>,
    server_message::Msg,
) {
    let mut client = FleetClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tx.send(first).await.unwrap();
    let mut stream = client
        .session(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let reply = stream.message().await.unwrap().unwrap().msg.unwrap();
    (tx, stream, reply)
}

fn expect_welcome(m: server_message::Msg) -> buckle_proto::v1::Welcome {
    match m {
        server_message::Msg::Welcome(w) => w,
        other => panic!("expected Welcome, got {other:?}"),
    }
}

fn expect_rejected(m: server_message::Msg) -> buckle_proto::v1::Rejected {
    match m {
        server_message::Msg::Rejected(r) => r,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn bootstrap_enrolment_issues_a_per_host_token() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let welcome = expect_welcome(reply);
    assert_eq!(welcome.status, "active");
    assert_eq!(welcome.issued_token.len(), 64);
    let hash = auth::hash_token(&welcome.issued_token);
    let stored = s
        .store
        .call(move |db| db.hosts().by_token_hash(&hash))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.id, "pi-1");
    assert_eq!(stored.platform.as_deref(), Some("linux"));

    // Reports are routed to the store.
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Facts(Facts {
            json: br#"{"os":{"family":"debian"}}"#.to_vec(),
            collected_at: None,
        })),
    })
    .await
    .unwrap();
    let mut facts = None;
    for _ in 0..100 {
        facts = s
            .store
            .call(|db| db.hosts().latest_facts("pi-1"))
            .await
            .unwrap();
        if facts.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(facts.unwrap().0["os"]["family"], "debian");
    drop(tx);

    // The issued token identifies the host from then on.
    let (_tx, _s2, reply) = open(s.addr, hello("pi-1", &welcome.issued_token, "2.0.1")).await;
    let again = expect_welcome(reply);
    assert!(again.issued_token.is_empty());
    // The bootstrap token cannot take over an enrolled identity.
    let (_tx, _s3, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    assert!(!expect_rejected(reply).retry);
}

#[tokio::test]
async fn approve_registration_leaves_new_hosts_pending() {
    let s = server(Registration::Approve, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-2", "boot", "2.0.0")).await;
    assert_eq!(expect_welcome(reply).status, "pending");
}

#[tokio::test]
async fn a_bad_token_is_rejected() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "not-a-token", "2.0.0")).await;
    let r = expect_rejected(reply);
    assert!(!r.retry);
    assert!(r.reason.contains("not recognised"));
}

#[tokio::test]
async fn an_agent_older_than_the_minimum_is_rejected() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "1.9.0")).await;
    let r = expect_rejected(reply);
    assert!(!r.retry);
    assert!(r.reason.contains("older"));
}

#[tokio::test]
async fn whois_authenticates_by_node_name() {
    let s = server(
        Registration::Auto,
        Some(Arc::new(FakeWhois)),
        vec![AgentAuth::Tailscale, AgentAuth::Token],
    )
    .await;
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", "", "2.0.0")).await;
    assert_eq!(expect_welcome(reply).status, "active");
    let host = s
        .store
        .call(|db| db.hosts().get("tail-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(host.auth_method.as_deref(), Some("tailscale"));
    let (_tx, _stream, reply) = open(s.addr, hello("impostor", "", "2.0.0")).await;
    assert!(expect_rejected(reply).reason.contains("tail-1"));
}

#[tokio::test]
async fn revisions_stream_in_chunks_with_total_and_digest() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let token = expect_welcome(reply).issued_token;

    let tree = s.data.path().join("tree");
    std::fs::create_dir_all(tree.join("modules/m/files")).unwrap();
    std::fs::write(tree.join("buckle.yaml"), "version: 2\n").unwrap();
    std::fs::write(tree.join("modules/m/module.yaml"), "name: m\n").unwrap();
    let big: Vec<u8> = (0u64..(2 * 1024 * 1024 + 17))
        .map(|i| (i * 7919 % 251) as u8)
        .collect();
    std::fs::write(tree.join("modules/m/files/blob.bin"), &big).unwrap();
    let builder = RevisionBuilder::new(s.store.clone(), s.revisions.clone(), EventBus::default());
    let rec = builder
        .build(
            CommitInfo {
                id: "abc".into(),
                author: "a".into(),
                message: "m".into(),
                time: chrono::Utc::now(),
            },
            tree,
            "git",
        )
        .await
        .unwrap();

    let mut client = FleetClient::connect(format!("http://{}", s.addr))
        .await
        .unwrap();
    let mut request = tonic::Request::new(FetchRevisionRequest {
        revision: "abc".into(),
    });
    request
        .metadata_mut()
        .insert(buckle_proto::TOKEN_METADATA_KEY, token.parse().unwrap());
    let mut stream = client.fetch_revision(request).await.unwrap().into_inner();
    let mut bytes = Vec::new();
    let mut first_total = None;
    let mut last_digest = String::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        first_total.get_or_insert(chunk.total);
        last_digest = chunk.digest.clone();
        bytes.extend_from_slice(&chunk.data);
    }
    assert_eq!(first_total, Some(bytes.len() as u64));
    assert_eq!(last_digest, rec.tarball_digest.clone().unwrap());
    assert_eq!(buckle_core::Digest::of(&bytes).to_string(), last_digest);

    let blob_digest = buckle_core::Digest::of(&big).to_string();
    let mut request = tonic::Request::new(buckle_proto::v1::FetchBlobRequest {
        digest: blob_digest.clone(),
    });
    request
        .metadata_mut()
        .insert(buckle_proto::TOKEN_METADATA_KEY, token.parse().unwrap());
    let mut stream = client.fetch_blob(request).await.unwrap().into_inner();
    let mut blob = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = stream.message().await.unwrap() {
        chunks += 1;
        blob.extend_from_slice(&chunk.data);
    }
    assert_eq!(chunks, 3, "1 MiB chunks");
    assert_eq!(blob, big);

    let anonymous = client
        .fetch_revision(FetchRevisionRequest {
            revision: "abc".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(anonymous.code(), tonic::Code::Unauthenticated);
}

/// The next Ack on `stream`, skipping other messages.
async fn next_ack(stream: &mut tonic::Streaming<ServerMessage>) -> Vec<u64> {
    loop {
        let m = tokio::time::timeout(std::time::Duration::from_secs(10), stream.message())
            .await
            .expect("an ack in time")
            .unwrap()
            .expect("the stream is open");
        if let Some(server_message::Msg::Ack(a)) = m.msg {
            return a.report_ids;
        }
    }
}

#[tokio::test]
async fn durable_reports_are_acknowledged_and_stored_once() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    let result = AgentMessage {
        msg: Some(agent_message::Msg::Result(buckle_proto::v1::ApplyResult {
            revision: "r1".into(),
            plan_digest: "d1".into(),
            succeeded: true,
            applied_revision: "r1".into(),
            report_id: 42,
            ..Default::default()
        })),
    };
    tx.send(result.clone()).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![42]);
    // A re-send (after a reconnect) is acknowledged again but stored once.
    tx.send(result).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![42]);
    let runs = s
        .store
        .call(|db| db.runs().for_host("pi-1", 10))
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);

    let rollback = AgentMessage {
        msg: Some(agent_message::Msg::Rollback(
            buckle_proto::v1::LocalRollback {
                from_revision: "r1".into(),
                to_revision: "r0".into(),
                reason: "cut off".into(),
                succeeded: false,
                rollback_error: "apt is locked".into(),
                report_id: 43,
                ..Default::default()
            },
        )),
    };
    tx.send(rollback.clone()).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![43]);
    tx.send(rollback).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![43]);
    let audits = s
        .store
        .call(|db| db.audit().for_subject("pi-1", 100))
        .await
        .unwrap();
    let rollbacks: Vec<_> = audits
        .iter()
        .filter(|a| a.action == "host.local_rollback")
        .collect();
    assert_eq!(rollbacks.len(), 1, "one audit row per report");
    assert!(rollbacks[0].message.contains("apt is locked"));
    assert!(rollbacks[0].message.contains("needs attention"));
}
