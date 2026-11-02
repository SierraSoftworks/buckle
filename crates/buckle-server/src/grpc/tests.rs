//! The gRPC front end to end with an in-process tonic client.

use super::*;
use crate::clock::TokioClock;
use crate::config::{AgentAuth, AgentsConfig, Registration};
use crate::events::EventBus;
use crate::grpc::SessionGuard;
use crate::rollout::{ControllerDeps, ControllerOptions, Sessions};
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::source::{CommitInfo, RevisionBuilder};
use crate::store::{NewHost, Store};
use async_trait::async_trait;
use buckle_api::HostStatus;
use buckle_core::secrets::InstallKey;
use buckle_proto::v1::fleet_client::FleetClient;
use buckle_proto::v1::{
    AgentMessage, Facts, FetchRevisionRequest, Hello, ServerMessage, agent_message, server_message,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// Says every peer is `tail-1`, carrying `tags` and owned by `user`.
struct FakeWhois {
    tags: Vec<String>,
    user: Option<String>,
}

impl FakeWhois {
    fn tagged(tag: &str) -> Arc<dyn Whois> {
        Arc::new(Self {
            tags: vec![tag.into()],
            user: None,
        })
    }

    fn owned_by(user: &str) -> Arc<dyn Whois> {
        Arc::new(Self {
            tags: vec![],
            user: Some(user.into()),
        })
    }
}

#[async_trait]
impl Whois for FakeWhois {
    fn covers(&self, _peer: &SocketAddr) -> bool {
        true
    }
    async fn whois(&self, _peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        Ok(Some(WhoisIdentity {
            node: "tail-1".into(),
            user: self.user.clone(),
            tags: self.tags.clone(),
        }))
    }
}

fn trusting(tags: &[&str], users: &[&str]) -> crate::config::TailscaleConfig {
    crate::config::TailscaleConfig {
        allowed_tags: tags.iter().map(|s| s.to_string()).collect(),
        allowed_users: users.iter().map(|s| s.to_string()).collect(),
    }
}

struct Server {
    addr: SocketAddr,
    store: Store,
    events: EventBus,
    data: tempfile::TempDir,
    revisions: Arc<RevisionCache>,
    guard: Arc<SessionGuard>,
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
    server_with(registration, whois, auth, SessionGuard::default()).await
}

async fn server_with(
    registration: Registration,
    whois: Option<Arc<dyn Whois>>,
    auth: Vec<AgentAuth>,
    guard: SessionGuard,
) -> Server {
    server_full(
        registration,
        whois,
        auth,
        guard,
        trusting(&["tag:buckle"], &[]),
    )
    .await
}

async fn server_full(
    registration: Registration,
    whois: Option<Arc<dyn Whois>>,
    auth: Vec<AgentAuth>,
    guard: SessionGuard,
    tailscale: crate::config::TailscaleConfig,
) -> Server {
    let guard = Arc::new(guard);
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
            tailscale,
            ..Default::default()
        },
        whois,
        events: events.clone(),
        clock,
        server_version: "2.0.0-test".into(),
        guard: guard.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(service, listener, None, cancel.clone()));
    Server {
        addr,
        store,
        events,
        data,
        revisions,
        guard,
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
async fn an_agent_older_than_the_minimum_is_rejected_and_can_enrol_once_upgraded() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "1.9.0")).await;
    let r = expect_rejected(reply);
    assert!(!r.retry);
    assert!(r.reason.contains("older"));
    let stored = s.store.call(|db| db.hosts().get("pi-1")).await.unwrap();
    assert!(stored.is_none(), "a refused agent leaves no record behind");

    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    assert_eq!(expect_welcome(reply).issued_token.len(), 64);
}

#[tokio::test]
async fn the_bootstrap_token_cannot_claim_a_host_registered_over_the_tailnet() {
    // A host the tailnet registered, then the same name over token auth.
    let plain = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let store = plain.store.clone();
    store
        .call(|db| {
            db.hosts().create(&NewHost {
                id: "tail-1".into(),
                status: HostStatus::Active,
                token_hash: None,
                auth_method: "tailscale".into(),
                at: chrono::Utc::now(),
            })?;
            Ok(())
        })
        .await
        .unwrap();
    let (_tx, _stream, reply) = open(plain.addr, hello("tail-1", "boot", "2.0.0")).await;
    let r = expect_rejected(reply);
    assert!(r.reason.contains("already registered"), "{}", r.reason);
    let host = store
        .call(|db| db.hosts().get("tail-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(host.token_hash, None, "no token was issued for it");
}

#[tokio::test]
async fn bootstrap_guesses_are_throttled_per_address() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    for _ in 0..crate::grpc::guard::ENROLMENT_ATTEMPTS_PER_MINUTE {
        let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "guess", "2.0.0")).await;
        assert!(expect_rejected(reply).reason.contains("not recognised"));
    }
    // Even the right token waits now.
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let r = expect_rejected(reply);
    assert!(r.retry);
    assert!(r.reason.contains("Too many"), "{}", r.reason);
}

#[tokio::test]
async fn a_stream_that_never_says_hello_is_closed() {
    let s = server_with(
        Registration::Auto,
        None,
        vec![AgentAuth::Token],
        SessionGuard {
            hello_timeout: std::time::Duration::from_millis(100),
            ..Default::default()
        },
    )
    .await;
    let mut client = FleetClient::connect(format!("http://{}", s.addr))
        .await
        .unwrap();
    let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(1);
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.session(ReceiverStream::new(rx)),
    )
    .await
    .expect("the server answers without a Hello")
    .unwrap()
    .into_inner();
    let reply = stream.message().await.unwrap().unwrap().msg.unwrap();
    assert!(expect_rejected(reply).reason.contains("Hello"));
}

#[tokio::test]
async fn closing_a_host_ends_its_live_session() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    assert!(s.guard.sessions.is_live("pi-1"));
    assert!(s.guard.sessions.close("pi-1"));
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
        .await
        .expect("the stream ends");
    assert!(matches!(end, Ok(None) | Err(_)), "{end:?}");
}

async fn fetch_revision_code(addr: SocketAddr, token: &str, revision: &str) -> tonic::Code {
    let mut client = FleetClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let mut request = tonic::Request::new(FetchRevisionRequest {
        revision: revision.into(),
    });
    request
        .metadata_mut()
        .insert(buckle_proto::TOKEN_METADATA_KEY, token.parse().unwrap());
    match client.fetch_revision(request).await {
        Ok(mut s) => match s.get_mut().message().await {
            Ok(_) => tonic::Code::Ok,
            Err(e) => e.code(),
        },
        Err(e) => e.code(),
    }
}

async fn build_revision(s: &Server, id: &str) -> crate::store::RevisionRecord {
    let tree = s.data.path().join(format!("tree-{id}"));
    std::fs::create_dir_all(tree.join("modules/m")).unwrap();
    std::fs::write(tree.join("buckle.yaml"), "version: 2\n").unwrap();
    std::fs::write(
        tree.join("modules/m/module.yaml"),
        format!("name: m\n# {id}\n"),
    )
    .unwrap();
    RevisionBuilder::new(s.store.clone(), s.revisions.clone(), EventBus::default())
        .build(
            CommitInfo {
                id: id.into(),
                author: "a".into(),
                message: "m".into(),
                time: chrono::Utc::now(),
            },
            tree,
            "git",
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn only_active_hosts_fetch_and_only_revisions_meant_for_them() {
    let s = server(Registration::Approve, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let welcome = expect_welcome(reply);
    assert_eq!(welcome.status, "pending");
    let token = welcome.issued_token;
    build_revision(&s, "aaa").await;
    build_revision(&s, "bbb").await;
    s.store
        .call(|db| db.kv().set(crate::store::kv::keys::STABLE_REVISION, "aaa"))
        .await
        .unwrap();

    assert_eq!(
        fetch_revision_code(s.addr, &token, "aaa").await,
        tonic::Code::PermissionDenied,
        "a pending host fetches nothing"
    );
    s.store
        .call(|db| db.hosts().set_status("pi-1", HostStatus::Active))
        .await
        .unwrap();
    assert_eq!(
        fetch_revision_code(s.addr, &token, "aaa").await,
        tonic::Code::Ok
    );
    assert_eq!(
        fetch_revision_code(s.addr, &token, "bbb").await,
        tonic::Code::PermissionDenied,
        "a revision nobody assigned it"
    );
    s.store
        .call(|db| db.hosts().set_status("pi-1", HostStatus::Paused))
        .await
        .unwrap();
    assert_eq!(
        fetch_revision_code(s.addr, &token, "aaa").await,
        tonic::Code::PermissionDenied,
        "a paused host fetches nothing"
    );
}

#[tokio::test]
async fn whois_authenticates_by_node_name() {
    let s = server(
        Registration::Auto,
        Some(FakeWhois::tagged("tag:buckle")),
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
async fn a_tailnet_node_cannot_take_over_a_token_enrolled_host_by_name() {
    let s = server(
        Registration::Auto,
        Some(FakeWhois::tagged("tag:buckle")),
        vec![AgentAuth::Tailscale, AgentAuth::Token],
    )
    .await;
    s.store
        .call(|db| {
            db.hosts().create(&NewHost {
                id: "tail-1".into(),
                status: HostStatus::Active,
                token_hash: Some(auth::hash_token("the-host-token")),
                auth_method: "token".into(),
                at: chrono::Utc::now(),
            })?;
            Ok(())
        })
        .await
        .unwrap();
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", "", "2.0.0")).await;
    assert!(
        expect_rejected(reply).reason.contains("No token"),
        "the node name alone is not enough"
    );
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", "the-host-token", "2.0.0")).await;
    assert_eq!(expect_welcome(reply).status, "active");
}

#[rstest::rstest]
#[case::tag_without_prefix(FakeWhois::tagged("tag:buckle"), trusting(&["buckle"], &[]), Some("active"))]
#[case::listed_owner(FakeWhois::owned_by("ada@example.com"), trusting(&[], &["ada@example.com"]), Some("active"))]
#[case::other_tag(FakeWhois::tagged("tag:laptop"), trusting(&["tag:buckle"], &[]), None)]
#[case::shared_in_user(FakeWhois::owned_by("eve@elsewhere.example"), trusting(&["tag:buckle"], &["ada@example.com"]), None)]
#[case::nothing_configured(FakeWhois::tagged("tag:buckle"), trusting(&[], &[]), Some("pending"))]
#[tokio::test]
async fn tailnet_nodes_are_trusted_only_by_tag_or_owner(
    #[case] whois: Arc<dyn Whois>,
    #[case] trust: crate::config::TailscaleConfig,
    #[case] expected: Option<&str>,
) {
    let s = server_full(
        Registration::Auto,
        Some(whois),
        vec![AgentAuth::Tailscale],
        SessionGuard::default(),
        trust,
    )
    .await;
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", "", "2.0.0")).await;
    match expected {
        Some(status) => assert_eq!(expect_welcome(reply).status, status),
        None => {
            assert!(expect_rejected(reply).reason.contains("not trusted"));
            let host = s.store.call(|db| db.hosts().get("tail-1")).await.unwrap();
            assert!(host.is_none(), "an untrusted node is not registered");
        }
    }
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
    s.store
        .call(|db| db.kv().set(crate::store::kv::keys::STABLE_REVISION, "abc"))
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

fn apply_event(digest: &str, event: buckle_core::engine::ApplyEvent) -> AgentMessage {
    AgentMessage {
        msg: Some(agent_message::Msg::Apply(buckle_proto::v1::ApplyEvent {
            revision: "r1".into(),
            plan_digest: digest.into(),
            event_json: serde_json::to_vec(&event).unwrap(),
            ..Default::default()
        })),
    }
}

async fn next_progress(
    rx: &mut tokio::sync::broadcast::Receiver<buckle_api::LiveEvent>,
) -> buckle_api::RunProgress {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a live event")
            .expect("the bus is open");
        if let buckle_api::LiveEvent::RunProgress { progress } = event {
            return progress;
        }
    }
}

#[tokio::test]
async fn apply_events_open_a_run_that_the_result_completes() {
    use buckle_core::engine::ApplyEvent;
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    tx.send(apply_event(
        "d1",
        ApplyEvent::ModuleStarted { module: "m".into() },
    ))
    .await
    .unwrap();
    let first = next_progress(&mut live).await;
    assert_eq!(first.module.as_deref(), Some("m"));
    assert_eq!(first.module_state, Some(buckle_api::ModuleState::Running));
    tx.send(apply_event(
        "d1",
        ApplyEvent::StepStarted {
            module: "m".into(),
            task_id: "t".into(),
            id: "file.copy[/x]".into(),
            kind: "file.copy".into(),
        },
    ))
    .await
    .unwrap();
    let step = next_progress(&mut live).await;
    assert_eq!(step.run_id, first.run_id, "one run for the apply");
    assert_eq!(step.step.as_deref(), Some("file.copy[/x]"));
    assert_eq!(step.step_state, Some(buckle_api::StepState::Running));

    let run_id = first.run_id.clone();
    let open_run = s
        .store
        .call(|db| db.runs().open_for_host("pi-1"))
        .await
        .unwrap()
        .expect("a running run");
    assert_eq!(open_run.id, run_id);
    let events = s
        .store
        .call(move |db| db.runs().events(&open_run.id))
        .await
        .unwrap();
    assert_eq!(events.len(), 2);

    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Result(buckle_proto::v1::ApplyResult {
            revision: "r1".into(),
            plan_digest: "d1".into(),
            succeeded: false,
            error: "boom".into(),
            report_id: 9,
            outcome_json: br#"{"plan_digest":"d1","succeeded":false,"changes":0,"removals":0,"duration_ms":5,"module_results":[],"failed_module":"m","rolled_back_modules":[],"not_started":[],"health":null,"error":"boom","error_detail":{"kind":"user","message":"boom","advice":["try again"]}}"#.to_vec(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![9]);
    let done = next_progress(&mut live).await;
    assert_eq!(done.run_id, run_id);
    assert_eq!(done.run_state, buckle_api::RunState::Failed);
    assert_eq!(
        done.error.map(|e| e.advice),
        Some(vec!["try again".to_string()])
    );
    let (open, runs) = s
        .store
        .call(|db| {
            Ok((
                db.runs().open_for_host("pi-1")?,
                db.runs().for_host("pi-1", 10)?,
            ))
        })
        .await
        .unwrap();
    assert!(open.is_none(), "the result closed the run");
    assert_eq!(runs.len(), 1, "the result completed the open run");
    assert_eq!(runs[0].id, run_id);
    assert!(runs[0].outcome_json.is_some());

    // The next apply of another plan is a new run.
    tx.send(apply_event(
        "d2",
        ApplyEvent::ModuleStarted { module: "m".into() },
    ))
    .await
    .unwrap();
    assert_ne!(next_progress(&mut live).await.run_id, run_id);
}

#[tokio::test]
async fn an_apply_result_larger_than_tonics_default_limit_is_accepted() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    // A host with thousands of steps: well over tonic's 4 MiB default.
    let outcome = serde_json::json!({ "padding": "x".repeat(6 * 1024 * 1024) });
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Result(buckle_proto::v1::ApplyResult {
            revision: "r1".into(),
            plan_digest: "d1".into(),
            succeeded: true,
            report_id: 7,
            outcome_json: serde_json::to_vec(&outcome).unwrap(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![7]);
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
    // Acked only after the controller saved its reaction, which removes the
    // pending copy kept for replay.
    let pending = s
        .store
        .call(|db| {
            db.kv()
                .with_prefix(crate::store::kv::keys::PENDING_REPORT_PREFIX)
        })
        .await
        .unwrap();
    assert!(pending.is_empty(), "{pending:?}");
}

/// A re-sent durable report that is still pending (stored, but the
/// controller never saved its reaction, e.g. the server stopped in between)
/// is handed to the controller again rather than dropped as a duplicate,
/// and acknowledged once its reaction is saved.
#[tokio::test]
async fn a_resent_report_still_pending_reaches_the_controller_again() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    let result = buckle_proto::v1::ApplyResult {
        revision: "r1".into(),
        plan_digest: "d1".into(),
        succeeded: true,
        applied_revision: "r1".into(),
        report_id: 9,
        ..Default::default()
    };
    // As if an earlier arrival was stored and the server stopped before the
    // controller reacted: the arrival is recorded and the copy is pending.
    let msg = agent_message::Msg::Result(result.clone());
    let (key, value) = (
        crate::rollout::pending_report_key("pi-1", 9),
        crate::rollout::encode_pending_report("pi-1", 9, &msg),
    );
    let now = chrono::Utc::now();
    s.store
        .call(move |db| {
            db.reports().first_arrival("pi-1", 9, "apply_result", now)?;
            db.kv().set(&key, &value)
        })
        .await
        .unwrap();
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Result(result)),
    })
    .await
    .unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![9]);
    let pending = s
        .store
        .call(|db| {
            db.kv()
                .with_prefix(crate::store::kv::keys::PENDING_REPORT_PREFIX)
        })
        .await
        .unwrap();
    assert!(
        pending.is_empty(),
        "the controller consumed it: {pending:?}"
    );
}
