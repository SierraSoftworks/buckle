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
        provisioning: None,
    }
}

struct Server {
    addr: SocketAddr,
    store: Store,
    events: EventBus,
    data: tempfile::TempDir,
    revisions: Arc<RevisionCache>,
    guard: Arc<SessionGuard>,
    upgrades: Arc<crate::upgrades::AgentUpgrades>,
    joins: Arc<crate::join::Joins>,
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
        AgentsConfig {
            tailscale: trusting(&["tag:buckle"], &[]),
            ..Default::default()
        },
    )
    .await
}

async fn server_full(
    registration: Registration,
    whois: Option<Arc<dyn Whois>>,
    auth: Vec<AgentAuth>,
    guard: SessionGuard,
    agents: AgentsConfig,
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
    let joins = Arc::new(crate::join::Joins::default());
    let upgrades = Arc::new(crate::upgrades::AgentUpgrades::new(
        sessions.clone(),
        "2.3.0",
    ));
    let service = FleetService {
        store: store.clone(),
        sessions: sessions.clone(),
        controller,
        revisions: revisions.clone(),
        secrets,
        agents: AgentsConfig {
            auth,
            bootstrap_token: Some("boot".into()),
            registration,
            minimum_agent_version: Some("2.0.0".into()),
            ..agents
        },
        whois,
        events: events.clone(),
        clock,
        server_version: "2.0.0-test".into(),
        guard: guard.clone(),
        upgrades: upgrades.clone(),
        joins: joins.clone(),
        identity: Arc::new(
            crate::identity::Issuer::load(
                &data.path().join("identity"),
                &Default::default(),
                "buckle".into(),
            )
            .unwrap(),
        ),
        workload: Default::default(),
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
        upgrades,
        joins,
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
            target: String::new(),
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

/// A join token from a command for `host` (any host when `None`).
async fn join_token(s: &Server, method: buckle_api::JoinMethod, host: Option<&str>) -> String {
    let ticket = s
        .joins
        .create(
            buckle_api::JoinRequest {
                platform: buckle_api::JoinPlatform::Linux,
                method,
                host_id: host.map(str::to_owned),
                labels: Default::default(),
                tailscale_tags: vec![],
            },
            "ada@example.com",
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let buckle_api::JoinState::Ready { command } = ticket.state else {
        panic!("a token join is ready at once");
    };
    let rest = command.split("--token ").nth(1).unwrap();
    rest.split_whitespace().next().unwrap().to_owned()
}

#[tokio::test]
async fn a_join_token_enrols_its_host_once_without_approval() {
    let s = server(Registration::Approve, None, vec![AgentAuth::Token]).await;
    let token = join_token(&s, buckle_api::JoinMethod::Token, Some("pi-3")).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-9", &token, "2.0.0")).await;
    assert!(
        expect_rejected(reply).reason.contains("not recognised"),
        "another host"
    );
    let (_tx, _stream, reply) = open(s.addr, hello("pi-3", &token, "2.0.0")).await;
    let welcome = expect_welcome(reply);
    assert_eq!(
        welcome.status, "active",
        "the admin who made the command approved it"
    );
    assert_eq!(welcome.issued_token.len(), 64);
    let (_tx, _stream, reply) = open(s.addr, hello("pi-4", &token, "2.0.0")).await;
    assert!(!expect_rejected(reply).retry, "used once");
}

#[tokio::test]
async fn a_tailnet_node_with_a_join_token_skips_approval() {
    let s = server(
        Registration::Approve,
        Some(FakeWhois::tagged("tag:buckle")),
        vec![AgentAuth::Tailscale],
    )
    .await;
    let token = join_token(&s, buckle_api::JoinMethod::Token, None).await;
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", &token, "2.0.0")).await;
    assert_eq!(expect_welcome(reply).status, "active");
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
    build_revision_with(s, id, "version: 2\n").await
}

async fn build_revision_with(
    s: &Server,
    id: &str,
    buckle_yaml: &str,
) -> crate::store::RevisionRecord {
    let tree = s.data.path().join(format!("tree-{id}"));
    std::fs::create_dir_all(tree.join("modules/m")).unwrap();
    std::fs::write(tree.join("buckle.yaml"), buckle_yaml).unwrap();
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

/// A tailscaled that cannot be reached (restarted, stale socket).
struct FailingWhois;

#[async_trait]
impl Whois for FailingWhois {
    fn covers(&self, _peer: &SocketAddr) -> bool {
        true
    }
    async fn whois(&self, _peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        Err(buckle_core::error::wrap_system(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            "The local tailscaled could not be asked who a peer is.",
            &[],
        ))
    }
}

#[rstest::rstest]
#[case::tailnet_only(vec![AgentAuth::Tailscale])]
#[case::token_fallback_without_token(vec![AgentAuth::Tailscale, AgentAuth::Token])]
#[tokio::test]
async fn a_failed_whois_is_retried_not_refused(#[case] auth: Vec<AgentAuth>) {
    let s = server(Registration::Auto, Some(Arc::new(FailingWhois)), auth).await;
    let (_tx, _stream, reply) = open(s.addr, hello("tail-1", "", "2.0.0")).await;
    let rejected = expect_rejected(reply);
    assert!(rejected.retry, "a server-side whois failure is retryable");
    assert!(
        rejected.reason.contains("tailscaled"),
        "{}",
        rejected.reason
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
        AgentsConfig {
            tailscale: trust,
            ..Default::default()
        },
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
async fn health_readings_belong_to_the_rollout_that_applied_their_revision() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    tx.send(module_started("d1")).await.unwrap();
    let run = next_progress(&mut live).await.run_id;
    s.store
        .call(move |db| db.runs().set_rollout(&run, "ro-1"))
        .await
        .unwrap();
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Health(buckle_proto::v1::HealthReport {
            scope: buckle_proto::v1::HealthScope::Drift as i32,
            revision: "r1".into(),
            results: vec![buckle_proto::v1::HealthResult {
                module: "m".into(),
                check: "api".into(),
                kind: "http".into(),
                required: true,
                passed: false,
                reason: "503".into(),
                ..Default::default()
            }],
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let checks = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let checks = s.store.call(|db| db.health().checks("pi-1")).await.unwrap();
            if !checks.is_empty() {
                return checks;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the readings are stored");
    assert_eq!(checks[0].status, "fail");
    let epoch = &checks[0].history[0];
    assert_eq!(
        (epoch.rollout_id.as_deref(), epoch.revision.as_str()),
        (Some("ro-1"), "r1")
    );
    let events = s
        .store
        .call(|db| db.health().for_host("pi-1", 10))
        .await
        .unwrap();
    assert_eq!(events[0].rollout.as_deref(), Some("ro-1"));
}

fn module_started(digest: &str) -> AgentMessage {
    apply_event(
        digest,
        buckle_core::engine::ApplyEvent::ModuleStarted { module: "m".into() },
    )
}

fn result_for(digest: &str, report_id: u64) -> AgentMessage {
    AgentMessage {
        msg: Some(agent_message::Msg::Result(buckle_proto::v1::ApplyResult {
            revision: "r1".into(),
            plan_digest: digest.into(),
            succeeded: true,
            report_id,
            ..Default::default()
        })),
    }
}

async fn run_of(s: &Server, id: &str) -> crate::store::RunRecord {
    let id = id.to_owned();
    s.store
        .call(move |db| db.runs().get("pi-1", &id))
        .await
        .unwrap()
        .expect("the run")
}

fn with_unreported(mut hello: AgentMessage, revisions: &[&str]) -> AgentMessage {
    if let Some(agent_message::Msg::Hello(h)) = &mut hello.msg {
        h.unreported_apply_revisions = revisions.iter().map(|r| r.to_string()).collect();
    }
    hello
}

#[tokio::test]
async fn a_new_session_closes_a_run_it_does_not_report_and_a_reapply_gets_a_fresh_run() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let token = expect_welcome(reply).issued_token;
    tx.send(module_started("d1")).await.unwrap();
    let first = next_progress(&mut live).await.run_id;
    drop((tx, stream));

    // The agent restarted: its Hello names no unreported apply.
    let (tx, _stream, reply) = open(s.addr, hello("pi-1", &token, "2.0.0")).await;
    expect_welcome(reply);
    let closed = next_progress(&mut live).await;
    assert_eq!(closed.run_id, first);
    assert_eq!(closed.run_state, buckle_api::RunState::Failed);
    let abandoned = run_of(&s, &first).await;
    assert_eq!(abandoned.outcome, "failed");
    assert_eq!(abandoned.error.as_deref(), Some(crate::store::ABANDONED));

    // Re-applying the same plan is a new run, not the first attempt's.
    tx.send(module_started("d1")).await.unwrap();
    let second = next_progress(&mut live).await.run_id;
    assert_ne!(second, first);
}

#[tokio::test]
async fn a_new_session_keeps_a_run_it_reports_as_still_running() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let token = expect_welcome(reply).issued_token;
    tx.send(module_started("d1")).await.unwrap();
    let first = next_progress(&mut live).await.run_id;
    drop((tx, stream));
    let (tx, _stream, reply) = open(
        s.addr,
        with_unreported(hello("pi-1", &token, "2.0.0"), &["r1"]),
    )
    .await;
    expect_welcome(reply);
    tx.send(module_started("d1")).await.unwrap();
    assert_eq!(
        next_progress(&mut live).await.run_id,
        first,
        "the same run goes on"
    );
    assert!(run_of(&s, &first).await.is_running());
}

#[tokio::test]
async fn a_result_for_another_plan_closes_the_open_run_which_its_own_result_still_completes() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    tx.send(module_started("d1")).await.unwrap();
    let open_run = next_progress(&mut live).await.run_id;
    tx.send(result_for("d2", 1)).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![1]);
    let abandoned = run_of(&s, &open_run).await;
    assert_eq!(abandoned.error.as_deref(), Some(crate::store::ABANDONED));
    // Its own result, late, still completes it.
    tx.send(result_for("d1", 2)).await.unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![2]);
    let done = run_of(&s, &open_run).await;
    assert_eq!(done.outcome, "succeeded");
    assert_eq!(done.error, None);
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

async fn wait_upgrade(
    s: &Server,
    what: &str,
    pred: impl Fn(Option<buckle_api::AgentUpgrade>) -> bool,
) {
    for _ in 0..100 {
        if pred(s.upgrades.get("pi-1")) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{what}: {:?}", s.upgrades.get("pi-1"));
}

async fn next_upgrade(stream: &mut tonic::Streaming<ServerMessage>) -> buckle_proto::v1::Upgrade {
    loop {
        match stream.message().await.unwrap().unwrap().msg {
            Some(server_message::Msg::Upgrade(u)) => return u,
            Some(_) => continue,
            None => panic!("the stream ended"),
        }
    }
}

#[tokio::test]
async fn a_requested_upgrade_is_resent_until_the_agent_reconnects_on_it() {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.2.0")).await;
    let token = expect_welcome(reply).issued_token;
    s.upgrades
        .request("pi-1", Some("2.2.0"), None, "ben", chrono::Utc::now())
        .unwrap();
    let sent = next_upgrade(&mut stream).await;
    assert_eq!((sent.version.as_str(), sent.when_idle), ("2.3.0", true));
    drop((tx, stream));

    // Restarted before upgrading: the new session is asked again.
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", &token, "2.2.0")).await;
    expect_welcome(reply);
    assert_eq!(next_upgrade(&mut stream).await.version, "2.3.0");
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Upgrade(
            buckle_proto::v1::UpgradeResult {
                version: "2.3.0".into(),
                outcome: buckle_proto::v1::UpgradeOutcome::Failed as i32,
                error: "no binary for this platform".into(),
            },
        )),
    })
    .await
    .unwrap();
    wait_upgrade(&s, "the failure is kept", |u| {
        u.is_some_and(|u| {
            u.state == buckle_api::UpgradeState::Failed
                && u.error.as_deref() == Some("no binary for this platform")
        })
    })
    .await;
    drop((tx, stream));

    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", &token, "2.3.0")).await;
    expect_welcome(reply);
    wait_upgrade(&s, "the reconnect settles it", |u| u.is_none()).await;
    for action in ["host.upgrade_failed", "host.upgraded"] {
        let mut found = false;
        for _ in 0..100 {
            let filter = crate::store::AuditFilter {
                filter: Some(buckle_core::Filter::new(format!("action == \"{action}\"")).unwrap()),
                limit: 10,
                ..Default::default()
            };
            found = s
                .store
                .call(move |db| db.audit().list(&filter))
                .await
                .unwrap()
                .1
                > 0;
            if found {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(found, "{action} is audited");
    }
}

// ------------------------------------------------------------ identity issuance

const CLUSTERS: &str = r#"version: 2
fleet:
  clusters:
    home-nomad:
      kind: nomad
      nodes: 'host.labels.nomad == true'
      executors: 'host.labels.role == "server"'
    lab-k3s:
      kind: kubernetes
      nodes: 'true'
      identity: local
"#;

/// An active host `id` with `labels`, enrolled by token, on stable revision `abc`.
async fn identity_host(labels: &[(&str, &str)]) -> (Server, String) {
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    let token = expect_welcome(reply).issued_token;
    build_revision_with(&s, "abc", CLUSTERS).await;
    let labels: std::collections::BTreeMap<String, String> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    s.store
        .call(move |db| {
            db.kv()
                .set(crate::store::kv::keys::STABLE_REVISION, "abc")?;
            db.hosts().set_labels("pi-1", &labels).map(|_| ())
        })
        .await
        .unwrap();
    (s, token)
}

async fn issue(
    s: &Server,
    token: &str,
    target: &str,
    purpose: buckle_proto::v1::IdentityPurpose,
) -> Result<buckle_proto::v1::IssuedIdentity, tonic::Status> {
    let mut client = FleetClient::connect(format!("http://{}", s.addr))
        .await
        .unwrap();
    let mut request = tonic::Request::new(buckle_proto::v1::IssueIdentityRequest {
        target: target.into(),
        purpose: purpose as i32,
        ..Default::default()
    });
    request
        .metadata_mut()
        .insert(buckle_proto::TOKEN_METADATA_KEY, token.parse().unwrap());
    client.issue_identity(request).await.map(|r| r.into_inner())
}

async fn audit_actions(s: &Server) -> Vec<(String, String)> {
    s.store
        .call(|db| {
            Ok(db
                .audit()
                .list(&crate::store::AuditFilter {
                    limit: 100,
                    ..Default::default()
                })?
                .0
                .into_iter()
                .filter(|e| e.action.starts_with("identity."))
                .map(|e| (e.action, e.subject.unwrap_or_default()))
                .collect())
        })
        .await
        .unwrap()
}

#[rstest::rstest]
#[case::executor_deploys(&[("role", "server"), ("nomad", "true")], "home-nomad", buckle_proto::v1::IdentityPurpose::Deploy, tonic::Code::Ok)]
#[case::node_drains(&[("nomad", "true")], "home-nomad", buckle_proto::v1::IdentityPurpose::Drain, tonic::Code::Ok)]
#[case::node_cannot_deploy(&[("nomad", "true")], "home-nomad", buckle_proto::v1::IdentityPurpose::Deploy, tonic::Code::PermissionDenied)]
#[case::outsider_cannot_drain(&[("role", "server")], "home-nomad", buckle_proto::v1::IdentityPurpose::Drain, tonic::Code::PermissionDenied)]
#[case::local_identity(&[], "lab-k3s", buckle_proto::v1::IdentityPurpose::Read, tonic::Code::PermissionDenied)]
#[case::undeclared(&[("role", "server")], "nowhere", buckle_proto::v1::IdentityPurpose::Read, tonic::Code::NotFound)]
#[case::no_purpose(&[("role", "server")], "home-nomad", buckle_proto::v1::IdentityPurpose::Unspecified, tonic::Code::InvalidArgument)]
#[tokio::test]
async fn identities_are_granted_by_the_cluster_selectors_and_audited(
    #[case] labels: &[(&str, &str)],
    #[case] target: &str,
    #[case] purpose: buckle_proto::v1::IdentityPurpose,
    #[case] expected: tonic::Code,
) {
    let (s, token) = identity_host(labels).await;
    let got = issue(&s, &token, target, purpose).await;
    assert_eq!(
        got.as_ref()
            .map_err(|e| e.code())
            .err()
            .unwrap_or(tonic::Code::Ok),
        expected,
        "{got:?}"
    );
    let audit = audit_actions(&s).await;
    match expected {
        tonic::Code::Ok => {
            let issued = got.unwrap();
            assert_eq!(issued.audience, target);
            assert!(!issued.token.is_empty());
            assert_eq!(
                audit,
                vec![("identity.issued".to_string(), target.to_string())]
            );
        }
        tonic::Code::InvalidArgument => assert!(audit.is_empty()),
        _ => assert_eq!(
            audit,
            vec![("identity.denied".to_string(), target.to_string())]
        ),
    }
}

#[tokio::test]
async fn identities_need_the_per_host_token() {
    let (s, _) = identity_host(&[("role", "server")]).await;
    let err = issue(
        &s,
        "wrong",
        "home-nomad",
        buckle_proto::v1::IdentityPurpose::Read,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[rstest::rstest]
#[case::executor(r#"host.labels.role == "server""#, tonic::Code::Ok)]
#[case::not_an_executor(r#"host.labels.role == "db""#, tonic::Code::PermissionDenied)]
#[tokio::test]
async fn clusters_created_through_the_api_grant_by_their_selectors(
    #[case] executors: &'static str,
    #[case] expected: tonic::Code,
) {
    let (s, token) = identity_host(&[("role", "server")]).await;
    s.store
        .call(move |db| {
            db.hosts()
                .create_cluster(&crate::store::NewCluster {
                    id: "edge".into(),
                    cluster: buckle_api::ClusterInfo {
                        spec: buckle_api::ClusterSpec {
                            kind: buckle_api::ClusterKind::Kubernetes,
                            description: String::new(),
                            api: None,
                            ui: None,
                            nodes: None,
                            executors: Some(executors.into()),
                            identity: "buckle_jwt".into(),
                            cluster: None,
                            scope: Default::default(),
                            trust: None,
                        },
                        declared: false,
                    },
                    agent_labels: Default::default(),
                    labels: Default::default(),
                    notes: String::new(),
                    status: HostStatus::Active,
                    at: chrono::Utc::now(),
                })
                .map(|_| ())
        })
        .await
        .unwrap();
    let got = issue(&s, &token, "edge", buckle_proto::v1::IdentityPurpose::Read).await;
    assert_eq!(
        got.map(|_| tonic::Code::Ok).unwrap_or_else(|e| e.code()),
        expected
    );
}

fn workload_hello(host: &str, token: &str) -> AgentMessage {
    AgentMessage {
        msg: Some(agent_message::Msg::Hello(Hello {
            host_id: host.into(),
            agent_version: "2.0.0".into(),
            workload_token: token.into(),
            capabilities: vec!["targets".into()],
            ..Default::default()
        })),
    }
}

/// A server accepting workload identities, with `home-nomad` trusting the
/// test cluster key.
async fn workload_server(auth: Vec<AgentAuth>) -> Server {
    use crate::workload::tests::{CLUSTER, jwks};
    let s = server(Registration::Approve, None, auth).await;
    let keys = jwks(&[&CLUSTER]);
    s.store
        .call(move |db| {
            db.hosts()
                .create_cluster(&crate::store::NewCluster {
                    id: "home-nomad".into(),
                    cluster: buckle_api::ClusterInfo {
                        spec: buckle_api::ClusterSpec {
                            kind: buckle_api::ClusterKind::Nomad,
                            description: String::new(),
                            api: None,
                            ui: None,
                            nodes: None,
                            executors: None,
                            identity: "workload".into(),
                            cluster: None,
                            scope: Default::default(),
                            trust: Some(Box::new(buckle_api::ClusterTrust {
                                keys: Some(keys),
                                ..Default::default()
                            })),
                        },
                        declared: false,
                    },
                    agent_labels: Default::default(),
                    labels: Default::default(),
                    notes: String::new(),
                    status: HostStatus::Active,
                    at: chrono::Utc::now(),
                })
                .map(|_| ())
        })
        .await
        .unwrap();
    s
}

fn executor_token(job: &str) -> String {
    use crate::workload::tests::{CLUSTER, mint, nomad_claims};
    mint(&CLUSTER, "k1", &nomad_claims(job))
}

#[tokio::test]
async fn an_in_cluster_executor_enrols_with_its_workload_identity() {
    let s = workload_server(vec![AgentAuth::Token, AgentAuth::WorkloadIdentity]).await;
    let token = executor_token("buckle-executor");
    let (tx, _stream, reply) = open(s.addr, workload_hello("home-nomad-executor", &token)).await;
    let first = expect_welcome(reply);
    // The cluster's trust is the operator's approval: no pending state.
    assert_eq!(first.status, "active");
    let host = s
        .store
        .call(|db| db.hosts().get("home-nomad-executor"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(host.executor_of(), Some("home-nomad"));
    assert_eq!(
        host.token_hash.as_deref(),
        Some(auth::hash_token(&first.issued_token).as_str()),
        "its other calls use the issued token"
    );
    drop(tx);

    let (_tx, _stream, reply) = open(s.addr, workload_hello("home-nomad-executor", &token)).await;
    let again = expect_welcome(reply);
    assert!(!again.issued_token.is_empty() && again.issued_token != first.issued_token);
}

#[rstest::rstest]
#[case::not_accepted(vec![AgentAuth::Token], "buckle-executor", "home-nomad-executor", "add `workload_identity`")]
#[case::another_job(vec![AgentAuth::WorkloadIdentity], "paste", "home-nomad-executor", "trusted subjects")]
#[case::a_hosts_name(vec![AgentAuth::Token, AgentAuth::WorkloadIdentity], "buckle-executor", "pi-1", "its own host_id")]
#[case::a_clusters_name(vec![AgentAuth::WorkloadIdentity], "buckle-executor", "home-nomad", "its own host_id")]
#[tokio::test]
async fn workload_identities_are_refused_with_a_reason(
    #[case] auth: Vec<AgentAuth>,
    #[case] job: &str,
    #[case] host: &str,
    #[case] reason: &str,
) {
    let s = workload_server(auth.clone()).await;
    if auth.contains(&AgentAuth::Token) {
        let (_tx, _stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
        expect_welcome(reply);
    }
    let (_tx, _stream, reply) = open(s.addr, workload_hello(host, &executor_token(job))).await;
    let rejected = expect_rejected(reply);
    assert!(rejected.reason.contains(reason), "{}", rejected.reason);
    assert!(!rejected.retry);
}

#[tokio::test]
async fn action_events_and_results_land_in_the_action_run_beside_an_open_apply() {
    use buckle_core::engine::ApplyEvent;
    let s = server(Registration::Auto, None, vec![AgentAuth::Token]).await;
    let mut live = s.events.subscribe();
    let (tx, mut stream, reply) = open(s.addr, hello("pi-1", "boot", "2.0.0")).await;
    expect_welcome(reply);
    // An apply's run is open; the action's run was started by the server.
    tx.send(module_started("d1")).await.unwrap();
    let apply = next_progress(&mut live).await.run_id;
    let action = crate::store::RunRecord {
        id: "act-1".into(),
        host: "pi-1".into(),
        rollout: None,
        revision: "r1".into(),
        plan_digest: String::new(),
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        outcome: crate::store::RUNNING.into(),
        changes: 0,
        removals: 0,
        error: None,
        trace_id: None,
        outcome_json: None,
        action: Some("web/restart".into()),
    };
    s.store
        .call(move |db| db.runs().add(&action).map(|_| ()))
        .await
        .unwrap();

    let mut event = apply_event(
        "ad",
        ApplyEvent::ModuleStarted {
            module: "web".into(),
        },
    );
    if let Some(agent_message::Msg::Apply(e)) = &mut event.msg {
        e.action_id = "act-1".into();
    }
    tx.send(event).await.unwrap();
    assert_eq!(next_progress(&mut live).await.run_id, "act-1");
    tx.send(AgentMessage {
        msg: Some(agent_message::Msg::Action(buckle_proto::v1::ActionResult {
            action_id: "act-1".into(),
            module: "web".into(),
            action: "restart".into(),
            revision: "r1".into(),
            succeeded: true,
            changes: 1,
            report_id: 11,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert_eq!(next_ack(&mut stream).await, vec![11]);
    let done = next_progress(&mut live).await;
    assert_eq!(
        (done.run_id.as_str(), done.run_state),
        ("act-1", buckle_api::RunState::Succeeded)
    );
    let finished = run_of(&s, "act-1").await;
    assert_eq!(
        (
            finished.outcome.as_str(),
            finished.changes,
            finished.action.as_deref()
        ),
        ("succeeded", 1, Some("web/restart"))
    );
    assert!(
        run_of(&s, &apply).await.is_running(),
        "the apply's run is untouched"
    );
    let events = s.store.call(|db| db.runs().events("act-1")).await.unwrap();
    assert_eq!(events.len(), 1);
}

/// The agent's own tailnet address, as a proxy forwards it.
const AGENT_ADDR: &str = "100.64.0.7:41234";

/// Knows the agent by its tailnet address and every other peer (the test's
/// loopback proxy) as the Tailscale Service host.
struct ProxiedWhois;

#[async_trait]
impl Whois for ProxiedWhois {
    fn covers(&self, _peer: &SocketAddr) -> bool {
        true
    }
    async fn whois(&self, peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        let (node, tag) = if peer.to_string() == AGENT_ADDR {
            ("tail-1", "tag:buckle")
        } else {
            ("connector", "tag:nomad-tailscale")
        };
        Ok(Some(WhoisIdentity {
            node: node.into(),
            user: None,
            tags: vec![tag.into()],
        }))
    }
}

/// A TCP proxy in front of `upstream` that opens each connection with a
/// PROXY v2 header naming `AGENT_ADDR`, or with none when `header` is off.
async fn tcp_proxy(upstream: SocketAddr, header: bool) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                if header {
                    let bytes = proxy::encode(AGENT_ADDR.parse().unwrap(), upstream);
                    server.write_all(&bytes).await.unwrap();
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    addr
}

async fn proxied_server(trusted: &[&str]) -> Server {
    server_full(
        Registration::Auto,
        Some(Arc::new(ProxiedWhois)),
        vec![AgentAuth::Tailscale],
        SessionGuard::default(),
        AgentsConfig {
            tailscale: trusting(&["tag:buckle"], &[]),
            trusted_proxies: trusted
                .iter()
                .map(|t| t.to_string().try_into().unwrap())
                .collect(),
            ..Default::default()
        },
    )
    .await
}

#[rstest::rstest]
#[case::by_tag(&["tag:nomad-tailscale"])]
#[case::by_address(&["127.0.0.0/8"])]
#[tokio::test]
async fn a_trusted_proxy_forwards_the_agents_identity(#[case] trusted: &[&str]) {
    let s = proxied_server(trusted).await;
    let addr = tcp_proxy(s.addr, true).await;
    let (_tx, _stream, reply) = open(addr, hello("tail-1", "", "2.0.0")).await;
    assert_eq!(expect_welcome(reply).status, "active");
    let audits = s
        .store
        .call(|db| db.audit().for_subject("tail-1", 10))
        .await
        .unwrap();
    let registered = audits
        .iter()
        .find(|a| a.action == "host.registered")
        .expect("the registration is audited");
    assert!(
        registered.message.contains("proxy"),
        "{}",
        registered.message
    );
}

#[tokio::test]
async fn a_trusted_proxy_without_a_header_is_its_own_peer() {
    let s = proxied_server(&["tag:nomad-tailscale"]).await;
    let addr = tcp_proxy(s.addr, false).await;
    let (_tx, _stream, reply) = open(addr, hello("tail-1", "", "2.0.0")).await;
    assert!(expect_rejected(reply).reason.contains("connector"));
}

#[rstest::rstest]
#[case::none_trusted(&[])]
#[case::other_tag(&["tag:elsewhere"])]
#[tokio::test]
async fn an_untrusted_peer_cannot_forward_an_address(#[case] trusted: &[&str]) {
    let s = proxied_server(trusted).await;
    let addr = tcp_proxy(s.addr, true).await;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(hello("tail-1", "", "2.0.0")).await.unwrap();
    let welcomed = match FleetClient::connect(format!("http://{addr}")).await {
        Ok(mut client) => match client.session(ReceiverStream::new(rx)).await {
            Ok(r) => matches!(
                r.into_inner().message().await,
                Ok(Some(ServerMessage {
                    msg: Some(server_message::Msg::Welcome(_))
                }))
            ),
            Err(_) => false,
        },
        Err(_) => false,
    };
    assert!(!welcomed, "the header is not read from an untrusted peer");
}
