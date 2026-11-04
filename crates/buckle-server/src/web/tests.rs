//! The HTTP API end to end through actix's test harness, including OIDC
//! sign-in against an in-process identity provider (wiremock) that serves
//! discovery and a JWKS for an RSA key generated here.

use super::*;
use crate::clock::TokioClock;
use crate::config::{OidcConfig, WebConfig};
use crate::rollout::{ControllerDeps, ControllerOptions, Sessions};
use crate::secrets::ServerSecrets;
use crate::store::NewHost;
use actix_web::body::MessageBody;
use actix_web::cookie::Cookie;
use actix_web::http::StatusCode;
use actix_web::test;
use base64::Engine as _;
use buckle_api::HostStatus;
use buckle_core::secrets::InstallKey;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// ------------------------------------------------------------ a health kind for the catalog

struct ProbeCheck;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ProbeSpec {
    target: String,
}

#[async_trait::async_trait]
impl buckle_core::health::HealthCheck for ProbeCheck {
    type Spec = ProbeSpec;
    const KIND: &'static str = "test.probe";
    const DESCRIPTION: &'static str = "A probe registered by the server's tests.";
    async fn check(
        &self,
        _cx: &buckle_core::task::Cx,
        _spec: &ProbeSpec,
    ) -> buckle_core::health::HealthStatus {
        buckle_core::health::HealthStatus::Pass
    }
}
buckle_core::register_health!(ProbeCheck);

// ------------------------------------------------------------ harness

struct Env {
    _data: tempfile::TempDir,
    store: Store,
    state: web::Data<AppState>,
    /// The controller's links to agents, for fake hosts.
    sessions: Arc<Sessions>,
}

async fn env(web_config: WebConfig) -> Env {
    env_with(web_config, |_| {}).await
}

async fn env_with(web_config: WebConfig, adjust: impl FnOnce(&mut AppState)) -> Env {
    let data = tempfile::tempdir().unwrap();
    let store = Store::in_memory().await.unwrap();
    let revisions = Arc::new(RevisionCache::new(data.path()));
    let events = EventBus::default();
    let sessions = Arc::new(Sessions::new());
    let (controller, _task) = crate::rollout::spawn(
        ControllerDeps {
            store: store.clone(),
            link: sessions.clone(),
            clock: Arc::new(TokioClock::new()),
            events: events.clone(),
            revisions: revisions.clone(),
            secrets: ServerSecrets::new(&Default::default(), InstallKey::from_bytes([2; 32])),
            approval_key: [2; 32],
            options: ControllerOptions::default(),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let mut state = AppState::new(
        store.clone(),
        revisions,
        controller,
        events,
        web_config,
        Some("hook-secret".into()),
        Some(Arc::new(Notify::new())),
        "2.0.0-test".into(),
    );
    state.upgrades = Arc::new(crate::upgrades::AgentUpgrades::new(
        sessions.clone(),
        "2.3.0",
    ));
    adjust(&mut state);
    let state = web::Data::new(state);
    Env {
        _data: data,
        store,
        state,
        sessions,
    }
}

fn acl(expr: &str) -> Option<buckle_core::Filter> {
    Some(buckle_core::Filter::new(expr).unwrap())
}

async fn add_host(store: &Store, id: &str, status: HostStatus) {
    let id = id.to_owned();
    store
        .call(move |db| {
            db.hosts().create(&NewHost {
                id,
                status,
                token_hash: None,
                auth_method: "token".into(),
                at: chrono::Utc::now(),
            })?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Fetches a CSRF token and returns (cookie, token).
async fn csrf<S, B>(app: &S) -> (Cookie<'static>, String)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: MessageBody,
{
    let resp = test::call_service(
        app,
        test::TestRequest::get().uri("/api/v1/csrf").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp
        .response()
        .cookies()
        .find(|c| c.name() == auth::CSRF_COOKIE)
        .unwrap()
        .into_owned();
    let body: buckle_api::CsrfToken = test::read_body_json(resp).await;
    (cookie, body.token)
}

// ------------------------------------------------------------ without OIDC

async fn me(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
    >,
    req: test::TestRequest,
) -> buckle_api::Me {
    let resp = test::call_service(app, req.uri("/api/v1/me").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    test::read_body_json(resp).await
}

#[actix_web::test]
async fn without_oidc_me_is_anonymous_and_says_whether_admin_acl_matched() {
    let open = env(WebConfig::default()).await;
    let app = test::init_service(app(open.state.clone())).await;
    let me_open = me(&app, test::TestRequest::get()).await;
    assert!(me_open.anonymous);
    assert!(
        me_open.is_admin,
        "no oidc and no admin_acl: everything is permitted"
    );

    let gated = env(WebConfig {
        admin_acl: acl(r#"headers.x-team == "ops""#),
        ..Default::default()
    })
    .await;
    let app = test::init_service(super::app(gated.state.clone())).await;
    let outsider = me(&app, test::TestRequest::get()).await;
    assert!(outsider.anonymous);
    assert!(!outsider.is_admin, "admin_acl still sees request metadata");
    let ops = me(
        &app,
        test::TestRequest::get().insert_header(("x-team", "ops")),
    )
    .await;
    assert!(ops.is_admin);
}

#[actix_web::test]
async fn acls_apply_to_request_metadata_without_oidc() {
    let e = env(WebConfig {
        user_acl: acl(r#"headers.x-team == "ops""#),
        ..Default::default()
    })
    .await;
    let app = test::init_service(app(e.state.clone())).await;
    let denied = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/fleet").to_request(),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let body: buckle_api::ErrorBody = test::read_body_json(denied).await;
    assert!(!body.error.is_empty());
    let allowed = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/fleet")
            .insert_header(("x-team", "ops"))
            .to_request(),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let fleet: buckle_api::Fleet = test::read_body_json(allowed).await;
    assert_eq!(fleet.counts.total, 0);
}

#[actix_web::test]
async fn without_oidc_an_absent_admin_acl_permits_admin_routes() {
    let e = env(WebConfig::default()).await;
    add_host(&e.store, "pi-1", HostStatus::Pending).await;
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "local development: sign-in is off"
    );
    let page: buckle_api::Page<buckle_api::AuditEntry> = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get().uri("/api/v1/audit").to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(page.items[0].actor, "anonymous");
}

#[actix_web::test]
async fn with_oidc_an_absent_admin_acl_means_nobody_is_admin() {
    let idp = idp().await;
    let e = env(WebConfig {
        admin_acl: None,
        ..oidc_web(&idp)
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Pending).await;
    let app = test::init_service(app(e.state.clone())).await;
    let session = Cookie::new(
        auth::SESSION_COOKIE,
        mint(&PROVIDER, &idp.server.uri(), None, &["admins"]),
    );
    let who = me(&app, test::TestRequest::get().cookie(session.clone())).await;
    assert!(!who.anonymous);
    assert!(!who.is_admin, "admin_acl absent with sign-in on: nobody");
    let (csrf_cookie, token) = csrf(&app).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(session)
            .cookie(csrf_cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[actix_web::test]
async fn mutations_need_the_csrf_token() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Pending).await;
    let app = test::init_service(app(e.state.clone())).await;
    let missing = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .to_request(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    let (cookie, token) = csrf(&app).await;
    let wrong = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, "not-the-token"))
            .to_request(),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let ok = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);
}

#[actix_web::test]
async fn hosts_can_be_relabelled_approved_and_previewed() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Pending).await;
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("tier".to_string(), "canary".to_string());
    let patched = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri("/api/v1/hosts/pi-1")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(buckle_api::HostPatch {
                labels: Some(labels.clone()),
                notes: Some("in the cupboard".into()),
                cluster: None,
            })
            .to_request(),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let summary: buckle_api::HostSummary = test::read_body_json(patched).await;
    assert_eq!(summary.effective_labels["tier"], "canary");
    assert_eq!(summary.notes, "in the cupboard");

    let approved = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .to_request(),
    )
    .await;
    let summary: buckle_api::HostSummary = test::read_body_json(approved).await;
    assert_eq!(summary.status, HostStatus::Active);
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);

    let preview = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/preview")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(buckle_api::HostPatch::default())
            .to_request(),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);

    let audit = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/audit?filter=action%20%3D%3D%20%22host.approve%22")
            .to_request(),
    )
    .await;
    let page: buckle_api::Page<buckle_api::AuditEntry> = test::read_body_json(audit).await;
    assert_eq!(page.total, 1);
    let bad = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/audit?filter=%3D%3D")
            .to_request(),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let missing = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/hosts/nope")
            .to_request(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn cluster_targets_are_declared_or_created_and_listed_beside_hosts() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    revision(
        &e,
        "r1",
        "version: 2\nfleet:\n  groups:\n    home: 'cluster.labels.site == \"home\"'\n  clusters:\n    home-nomad:\n      kind: nomad\n      nodes: 'host.labels.nomad == true'\n      labels: { site: home }\n",
    )
    .await;
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;

    // Listing the fleet materialises the declared cluster beside the hosts.
    let fleet: buckle_api::Fleet = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get().uri("/api/v1/fleet").to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(
        fleet
            .hosts
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        vec!["pi-1"]
    );
    assert_eq!(fleet.clusters.len(), 1);
    let declared = &fleet.clusters[0];
    assert_eq!(declared.id, "home-nomad");
    assert_eq!(declared.kind, buckle_api::TargetKind::Cluster);
    assert!(declared.cluster.as_ref().is_some_and(|c| c.declared));
    assert_eq!(declared.groups, vec!["home"]);
    assert_eq!((fleet.counts.total, fleet.counts.clusters), (1, 1));
    let hosts: Vec<buckle_api::HostSummary> = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get().uri("/api/v1/hosts").to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(hosts.len(), 1, "the host list stays hosts only");

    // A declared cluster cannot be forgotten or respecified through the API.
    let forget = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/api/v1/targets/home-nomad")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(forget.status(), StatusCode::CONFLICT);

    // An operator creates a cluster target; bad specs are refused.
    let spec = buckle_api::ClusterSpec {
        kind: buckle_api::ClusterKind::Kubernetes,
        description: String::new(),
        api: Some("https://10.0.0.1:6443".into()),
        nodes: Some("host.labels.k3s == true".into()),
        executors: None,
        identity: "local".into(),
        cluster: None,
        scope: Default::default(),
    };
    let create = |body: buckle_api::CreateTarget| {
        test::TestRequest::post()
            .uri("/api/v1/targets")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(body)
            .to_request()
    };
    let bad = test::call_service(
        &app,
        create(buckle_api::CreateTarget {
            id: "lab-k3s".into(),
            kind: buckle_api::TargetKind::Cluster,
            cluster: Some(buckle_api::ClusterSpec {
                nodes: Some("cluster.kind == \"nomad\"".into()),
                ..spec.clone()
            }),
            labels: Default::default(),
            notes: String::new(),
        }),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let taken = test::call_service(
        &app,
        create(buckle_api::CreateTarget {
            id: "pi-1".into(),
            kind: buckle_api::TargetKind::Cluster,
            cluster: Some(spec.clone()),
            labels: Default::default(),
            notes: String::new(),
        }),
    )
    .await;
    assert_eq!(taken.status(), StatusCode::CONFLICT);
    let created = test::call_service(
        &app,
        create(buckle_api::CreateTarget {
            id: "lab-k3s".into(),
            kind: buckle_api::TargetKind::Cluster,
            cluster: Some(spec.clone()),
            labels: [("site".to_string(), "lab".to_string())].into(),
            notes: "the lab".into(),
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let summary: buckle_api::HostSummary = test::read_body_json(created).await;
    assert_eq!(summary.kind, buckle_api::TargetKind::Cluster);
    assert_eq!(summary.effective_labels["site"], "lab");
    assert!(
        summary
            .cluster
            .as_ref()
            .is_some_and(|c| !c.declared && c.spec == spec)
    );

    // Its spec can be changed, and it can be paused and forgotten.
    let patched = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri("/api/v1/targets/lab-k3s")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(buckle_api::HostPatch {
                cluster: Some(buckle_api::ClusterSpec {
                    executors: Some("host.labels.k3s_server == true".into()),
                    ..spec.clone()
                }),
                ..Default::default()
            })
            .to_request(),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let summary: buckle_api::HostSummary = test::read_body_json(patched).await;
    assert_eq!(
        summary
            .cluster
            .as_ref()
            .and_then(|c| c.spec.executors.clone())
            .as_deref(),
        Some("host.labels.k3s_server == true")
    );
    let paused = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/targets/lab-k3s/pause")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(paused.status(), StatusCode::OK);
    let targets: Vec<buckle_api::HostSummary> = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get().uri("/api/v1/targets").to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(targets.len(), 3);
    let forgotten = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/api/v1/targets/lab-k3s")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(forgotten.status(), StatusCode::NO_CONTENT);
    let detail = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/targets/lab-k3s")
            .to_request(),
    )
    .await;
    assert_eq!(detail.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn admins_can_ask_a_connected_agent_to_upgrade() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    add_host(&e.store, "pi-2", HostStatus::Active).await;
    let (_, mut rx) = e.sessions.attach("pi-1");
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    let upgrade = |host: &str, version: Option<&str>| {
        test::TestRequest::post()
            .uri(&format!("/api/v1/hosts/{host}/upgrade"))
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(buckle_api::UpgradeRequest {
                version: version.map(str::to_owned),
            })
            .to_request()
    };

    for (host, version, status) in [
        ("pi-2", None, StatusCode::CONFLICT),
        ("pi-1", Some("latest"), StatusCode::BAD_REQUEST),
        ("nope", None, StatusCode::NOT_FOUND),
    ] {
        let resp = test::call_service(&app, upgrade(host, version)).await;
        assert_eq!(resp.status(), status, "{host} {version:?}");
    }
    assert!(rx.try_recv().is_err(), "refused requests send nothing");

    let resp = test::call_service(&app, upgrade("pi-1", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let summary: buckle_api::HostSummary = test::read_body_json(resp).await;
    let requested = summary.upgrade.expect("the request is shown on the host");
    assert_eq!(
        requested.version, "2.3.0",
        "defaults to the desired version"
    );
    assert_eq!(requested.state, buckle_api::UpgradeState::Requested);
    match rx.try_recv().map(|m| m.msg) {
        Ok(Some(buckle_proto::v1::server_message::Msg::Upgrade(u))) => {
            assert_eq!(u.version, "2.3.0");
            assert!(u.when_idle);
        }
        other => panic!("expected an Upgrade, got {other:?}"),
    }

    let fleet = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/fleet").to_request(),
    )
    .await;
    let fleet: buckle_api::Fleet = test::read_body_json(fleet).await;
    assert_eq!(fleet.desired_agent_version.as_deref(), Some("2.3.0"));
    assert!(
        fleet
            .hosts
            .iter()
            .any(|h| h.id == "pi-1" && h.upgrade.is_some())
    );

    let audit = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/audit?filter=action%20%3D%3D%20%22host.upgrade%22")
            .to_request(),
    )
    .await;
    let page: buckle_api::Page<buckle_api::AuditEntry> = test::read_body_json(audit).await;
    assert_eq!(page.total, 1);
}

#[actix_web::test]
async fn admins_create_join_commands_through_tailscale_consent() {
    let tailscale = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tskey-auth-1",
            "expires_in": 3600,
        })))
        .mount(&tailscale)
        .await;
    let e = env_with(
        WebConfig {
            admin_acl: acl("true"),
            ..Default::default()
        },
        |s| {
            let mut settings = crate::join::Joins::default().settings().clone();
            settings.methods = vec![buckle_api::JoinMethod::Tailscale];
            settings.tailscale = Some(crate::join::Provisioning {
                config: crate::config::TailscaleProvisioning {
                    flow: crate::config::ProvisioningFlow::App,
                    client_id: "c".into(),
                    client_secret: "s".into(),
                    tags: vec![],
                    node_attributes: vec![],
                    api_url: tailscale.uri(),
                    login_url: "https://login.example.com".into(),
                },
                tags: vec!["tag:buckle".into()],
                redirect_uri: "http://localhost/api/v1/join/tailscale/callback".into(),
            });
            s.joins = Arc::new(crate::join::Joins::new(settings));
        },
    )
    .await;
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;

    let options = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/join").to_request(),
    )
    .await;
    let options: buckle_api::JoinOptions = test::read_body_json(options).await;
    assert_eq!(options.tailscale.unwrap().tags, vec!["tag:buckle"]);

    let create = |method: buckle_api::JoinMethod| {
        test::TestRequest::post()
            .uri("/api/v1/join")
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(buckle_api::JoinRequest {
                platform: buckle_api::JoinPlatform::Linux,
                method,
                host_id: Some("web-1".into()),
                labels: Default::default(),
                tailscale_tags: vec![],
            })
            .to_request()
    };
    let refused = test::call_service(&app, create(buckle_api::JoinMethod::Token)).await;
    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "token auth is off"
    );
    let resp = test::call_service(&app, create(buckle_api::JoinMethod::Tailscale)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let ticket: buckle_api::JoinTicket = test::read_body_json(resp).await;
    let buckle_api::JoinState::Authorize { authorize_url } = &ticket.state else {
        panic!("expected consent first, got {:?}", ticket.state);
    };
    let state = url::Url::parse(authorize_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();

    let callback = |state: &str| {
        test::TestRequest::get()
            .uri(&format!(
                "/api/v1/join/tailscale/callback?state={state}&code=abc"
            ))
            .to_request()
    };
    let back = test::call_service(&app, callback(&state)).await;
    assert_eq!(back.status(), StatusCode::SEE_OTHER);
    let location = |r: &actix_web::dev::ServiceResponse<_>| {
        r.headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(location(&back), format!("/hosts?join={}", ticket.id));
    let again = test::call_service(&app, callback(&state)).await;
    assert_eq!(location(&again), "/hosts?join=expired");

    let ready = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/api/v1/join/{}", ticket.id))
            .to_request(),
    )
    .await;
    let ready: buckle_api::JoinTicket = test::read_body_json(ready).await;
    match ready.state {
        buckle_api::JoinState::Ready { command } => {
            assert!(command.contains("tskey-auth-1"), "{command}")
        }
        other => panic!("expected a command, got {other:?}"),
    }
}

#[actix_web::test]
async fn forgetting_a_host_closes_its_agent_session() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    let live = e.state.agent_sessions.register("pi-1", 1);
    let other = e.state.agent_sessions.register("pi-2", 2);
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/api/v1/hosts/pi-1")
            .cookie(cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(live.is_cancelled(), "the forgotten host's stream is closed");
    assert!(!other.is_cancelled());
}

async fn next_chunk(body: &mut actix_web::body::BoxBody) -> String {
    let chunk = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)),
    )
    .await
    .expect("a chunk arrives")
    .expect("the stream is open")
    .unwrap();
    String::from_utf8_lossy(&chunk).into_owned()
}

#[actix_web::test]
async fn sse_streams_controller_events() {
    let e = env(WebConfig {
        admin_acl: acl("true"),
        ..Default::default()
    })
    .await;
    // A valid revision with no hosts: starting it completes immediately.
    let dir = e.state.revisions.dir("r1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("buckle.yaml"), "version: 2\n").unwrap();
    let rec = crate::store::RevisionRecord {
        id: "r1".into(),
        commit: "r1".into(),
        author: "a".into(),
        message: "first".into(),
        committed_at: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        tarball_digest: Some("t".into()),
        status: buckle_api::RevisionStatus::Valid,
        errors: vec![],
        module_count: 0,
        source: "path".into(),
    };
    e.store
        .call(move |db| db.revisions().upsert(&rec))
        .await
        .unwrap();

    let app = test::init_service(app(e.state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/events").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let mut body = resp.into_body().boxed();
    assert!(next_chunk(&mut body).await.contains("connected"));

    let (cookie, token) = csrf(&app).await;
    let started = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/revisions/r1/rollout")
            .cookie(cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .set_json(buckle_api::StartRollout { note: "go".into() })
            .to_request(),
    )
    .await;
    assert_eq!(started.status(), StatusCode::CREATED);
    let mut seen = String::new();
    for _ in 0..20 {
        seen.push_str(&next_chunk(&mut body).await);
        if seen.contains("event: rollout_changed") {
            break;
        }
    }
    assert!(seen.contains("event: rollout_changed"), "{seen}");
    assert!(seen.contains("data: {"));
}

#[actix_web::test]
async fn the_catalog_lists_registered_kinds() {
    let e = env(WebConfig::default()).await;
    let app = test::init_service(app(e.state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/catalog").to_request(),
    )
    .await;
    let catalog: buckle_api::Catalog = test::read_body_json(resp).await;
    let probe = catalog
        .health
        .iter()
        .find(|k| k.kind == "test.probe")
        .expect("registered kind listed");
    assert_eq!(
        probe.description,
        "A probe registered by the server's tests."
    );
    assert!(probe.schema.is_object());
    assert_eq!(catalog.server_version, "2.0.0-test");
}

#[actix_web::test]
async fn webhooks_need_a_valid_signature() {
    use hmac::{Hmac, KeyInit, Mac};
    let e = env(WebConfig::default()).await;
    let app = test::init_service(app(e.state.clone())).await;
    let body = br#"{"ref":"refs/heads/main"}"#;
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"hook-secret").unwrap();
    mac.update(body);
    let good = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let ok = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/webhooks/source")
            .insert_header(("x-hub-signature-256", good))
            .set_payload(body.to_vec())
            .to_request(),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::ACCEPTED);
    let bad = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/webhooks/source")
            .insert_header(("x-hub-signature-256", "sha256=00"))
            .set_payload(body.to_vec())
            .to_request(),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn the_ui_falls_back_to_index_and_robots_disallow() {
    let e = env(WebConfig::default()).await;
    let app = test::init_service(app(e.state.clone())).await;
    let robots = test::call_service(
        &app,
        test::TestRequest::get().uri("/robots.txt").to_request(),
    )
    .await;
    assert_eq!(
        test::read_body(robots).await,
        "User-agent: *\nDisallow: /\n"
    );
    let page = test::call_service(
        &app,
        test::TestRequest::get().uri("/rollouts/123").to_request(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
}

// ------------------------------------------------------------ OIDC

const KID: &str = "buckle-test-key";
const CLIENT_ID: &str = "buckle-under-test";

struct ProviderKey {
    der: Vec<u8>,
    jwk: serde_json::Value,
}

fn rsa_key(kid: &str) -> ProviderKey {
    use rsa::pkcs1::EncodeRsaPrivateKey as _;
    use rsa::traits::PublicKeyParts as _;
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let der = key.to_pkcs1_der().unwrap().as_bytes().to_vec();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    ProviderKey {
        der,
        jwk: serde_json::json!({
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
            "n": b64.encode(key.n().to_bytes_be()),
            "e": b64.encode(key.e().to_bytes_be()),
        }),
    }
}

static PROVIDER: LazyLock<ProviderKey> = LazyLock::new(|| rsa_key(KID));
static FORGER: LazyLock<ProviderKey> = LazyLock::new(|| rsa_key(KID));

fn mint(key: &ProviderKey, issuer: &str, nonce: Option<&str>, groups: &[&str]) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.into());
    let now = chrono::Utc::now().timestamp();
    let mut claims = serde_json::json!({
        "iss": issuer,
        "aud": CLIENT_ID,
        "sub": "ada-123",
        "email": "ada@example.com",
        "name": "Ada Lovelace",
        "groups": groups,
        "iat": now,
        "exp": now + 3600,
    });
    if let Some(n) = nonce {
        claims["nonce"] = serde_json::Value::String(n.into());
    }
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_der(&key.der),
    )
    .unwrap()
}

/// The token endpoint: checks PKCE against the challenge the authorization
/// request carried and mints an id token with that request's nonce.
#[derive(Clone)]
struct TokenEndpoint {
    issuer: String,
    flows: Arc<Mutex<HashMap<String, (String, String)>>>,
    forge: Arc<Mutex<bool>>,
}

impl Respond for TokenEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        let Some((challenge, nonce)) = form
            .get("code")
            .and_then(|c| self.flows.lock().unwrap().get(c).cloned())
        else {
            return ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": "invalid_grant"}));
        };
        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
        if super::oidc::sha256_b64(&verifier) != challenge
            || form.get("client_secret").map(String::as_str) != Some("shh")
        {
            return ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": "invalid_grant"}));
        }
        let key: &ProviderKey = if *self.forge.lock().unwrap() {
            &FORGER
        } else {
            &PROVIDER
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id_token": mint(key, &self.issuer, Some(&nonce), &["admins"]),
            "refresh_token": "refresh-1",
            "expires_in": 3600,
            "token_type": "Bearer",
        }))
    }
}

struct Idp {
    server: MockServer,
    flows: Arc<Mutex<HashMap<String, (String, String)>>>,
    forge: Arc<Mutex<bool>>,
}

async fn idp() -> Idp {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"keys": [PROVIDER.jwk]})),
        )
        .mount(&server)
        .await;
    let flows = Arc::new(Mutex::new(HashMap::new()));
    let forge = Arc::new(Mutex::new(false));
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenEndpoint {
            issuer: issuer.clone(),
            flows: flows.clone(),
            forge: forge.clone(),
        })
        .mount(&server)
        .await;
    Idp {
        server,
        flows,
        forge,
    }
}

fn oidc_web(idp: &Idp) -> WebConfig {
    WebConfig {
        base_url: Some("http://buckle.test".into()),
        user_acl: acl(r#"claims.email endswith "@example.com""#),
        admin_acl: acl(r#"claims.groups contains "admins""#),
        oidc: Some(OidcConfig {
            endpoint: idp.server.uri(),
            client_id: CLIENT_ID.into(),
            client_secret: "shh".into(),
            scopes: vec!["openid".into(), "email".into()],
        }),
        ..Default::default()
    }
}

/// Runs login, plays the provider's authorize step, and returns the
/// callback response.
async fn sign_in<S, B>(app: &S, idp: &Idp, tamper_state: bool) -> actix_web::dev::ServiceResponse<B>
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: MessageBody,
{
    let login = test::call_service(
        app,
        test::TestRequest::get()
            .uri("/api/v1/auth/login?return_to=/rollouts")
            .to_request(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::FOUND);
    let location =
        url::Url::parse(login.headers().get("location").unwrap().to_str().unwrap()).unwrap();
    let q: HashMap<String, String> = location.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], "http://buckle.test/api/v1/auth/callback");
    let cookies: Vec<Cookie<'static>> =
        login.response().cookies().map(|c| c.into_owned()).collect();
    assert!(cookies.iter().all(|c| c.http_only() == Some(true)));
    idp.flows.lock().unwrap().insert(
        "the-code".into(),
        (q["code_challenge"].clone(), q["nonce"].clone()),
    );
    let state = if tamper_state {
        "tampered".to_string()
    } else {
        q["state"].clone()
    };
    let mut req = test::TestRequest::get().uri(&format!(
        "/api/v1/auth/callback?code=the-code&state={state}"
    ));
    for c in cookies {
        req = req.cookie(c);
    }
    test::call_service(app, req.to_request()).await
}

#[actix_web::test]
async fn oidc_sign_in_sets_a_session_that_me_and_the_acls_read() {
    let idp = idp().await;
    let e = env(oidc_web(&idp)).await;
    let app = test::init_service(app(e.state.clone())).await;

    let anonymous = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/fleet").to_request(),
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let me = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/me").to_request(),
    )
    .await;
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);

    let callback = sign_in(&app, &idp, false).await;
    assert_eq!(callback.status(), StatusCode::FOUND);
    assert_eq!(callback.headers().get("location").unwrap(), "/rollouts");
    let session = callback
        .response()
        .cookies()
        .find(|c| c.name() == auth::SESSION_COOKIE)
        .expect("a session cookie")
        .into_owned();
    assert_eq!(session.http_only(), Some(true));

    let me = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/me")
            .cookie(session.clone())
            .to_request(),
    )
    .await;
    assert_eq!(me.status(), StatusCode::OK);
    let me: buckle_api::Me = test::read_body_json(me).await;
    assert_eq!(me.email.as_deref(), Some("ada@example.com"));
    assert_eq!(me.name, "Ada Lovelace");
    assert!(me.is_admin);
    let fleet = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/fleet")
            .cookie(session)
            .to_request(),
    )
    .await;
    assert_eq!(fleet.status(), StatusCode::OK);
}

#[actix_web::test]
async fn forged_tokens_and_tampered_state_are_refused() {
    let idp = idp().await;
    let e = env(oidc_web(&idp)).await;
    let app = test::init_service(app(e.state.clone())).await;

    let tampered = sign_in(&app, &idp, true).await;
    assert_eq!(tampered.status(), StatusCode::BAD_REQUEST);

    *idp.forge.lock().unwrap() = true;
    let forged = sign_in(&app, &idp, false).await;
    assert_eq!(
        forged.status(),
        StatusCode::BAD_REQUEST,
        "an id token signed by another key is refused"
    );

    let cookie = Cookie::new(
        auth::SESSION_COOKIE,
        mint(&FORGER, &idp.server.uri(), None, &["admins"]),
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/me")
            .cookie(cookie)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn a_valid_session_failing_user_acl_is_403_and_admin_routes_need_admin_acl() {
    let idp = idp().await;
    let mut web_config = oidc_web(&idp);
    web_config.admin_acl = acl(r#"claims.groups contains "root""#);
    let e = env(web_config.clone()).await;
    add_host(&e.store, "pi-1", HostStatus::Pending).await;
    let app = test::init_service(app(e.state.clone())).await;
    let issuer = idp.server.uri();
    let user = Cookie::new(
        auth::SESSION_COOKIE,
        mint(&PROVIDER, &issuer, None, &["ops"]),
    );
    let fleet = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/fleet")
            .cookie(user.clone())
            .to_request(),
    )
    .await;
    assert_eq!(fleet.status(), StatusCode::OK);
    let (csrf_cookie, token) = csrf(&app).await;
    let approve = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/hosts/pi-1/approve")
            .cookie(user)
            .cookie(csrf_cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(approve.status(), StatusCode::FORBIDDEN);

    let mut strict = web_config;
    strict.user_acl = acl(r#"claims.email endswith "@other.example""#);
    let e2 = env(strict).await;
    let app2 = test::init_service(super::app(e2.state.clone())).await;
    let session = Cookie::new(auth::SESSION_COOKIE, mint(&PROVIDER, &issuer, None, &[]));
    let denied = test::call_service(
        &app2,
        test::TestRequest::get()
            .uri("/api/v1/fleet")
            .cookie(session)
            .to_request(),
    )
    .await;
    assert_eq!(
        denied.status(),
        StatusCode::FORBIDDEN,
        "authenticated but not allowed: 403, not 401"
    );
}

#[actix_web::test]
async fn every_protected_route_is_401_without_a_session() {
    let idp = idp().await;
    let e = env(oidc_web(&idp)).await;
    let app = test::init_service(app(e.state.clone())).await;
    for (m, uri) in super::routes::PROTECTED {
        let req = match *m {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "PATCH" => test::TestRequest::patch(),
            "DELETE" => test::TestRequest::delete(),
            other => panic!("unexpected method {other}"),
        };
        let resp = test::call_service(&app, req.uri(uri).to_request()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{m} {uri}");
    }
}

// ------------------------------------------------------------ hardening

#[actix_web::test]
async fn path_acls_see_the_decoded_path_and_ambiguous_paths_are_refused() {
    let e = env(WebConfig {
        user_acl: acl(r#"path != "/api/v1/fleet""#),
        ..Default::default()
    })
    .await;
    let app = test::init_service(app(e.state.clone())).await;
    for (uri, expected) in [
        ("/api/v1/fleet", StatusCode::FORBIDDEN),
        ("/api/v1/fl%65et", StatusCode::FORBIDDEN),
        ("/api/v1/hosts", StatusCode::OK),
        ("/api/v1/hosts%2Fpi-1", StatusCode::BAD_REQUEST),
        ("/api/v1/auth/%2e%2e/fleet", StatusCode::BAD_REQUEST),
        ("/%61pi/v1/fleet", StatusCode::FORBIDDEN),
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(resp.status(), expected, "{uri}");
    }
}

#[actix_web::test]
async fn signing_out_revokes_the_session_and_ends_its_event_stream() {
    let idp = idp().await;
    let e = env_with(oidc_web(&idp), |s| {
        s.sse_recheck = std::time::Duration::from_millis(100);
    })
    .await;
    let app = test::init_service(app(e.state.clone())).await;
    let session = Cookie::new(
        auth::SESSION_COOKIE,
        mint(&PROVIDER, &idp.server.uri(), None, &[]),
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/events")
            .cookie(session.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().boxed();
    assert!(next_chunk(&mut body).await.contains("connected"));

    let (csrf_cookie, token) = csrf(&app).await;
    let out = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/auth/logout")
            .cookie(session.clone())
            .cookie(csrf_cookie)
            .insert_header((auth::CSRF_HEADER, token))
            .to_request(),
    )
    .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);

    let again = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/fleet")
            .cookie(session)
            .to_request(),
    )
    .await;
    assert_eq!(
        again.status(),
        StatusCode::UNAUTHORIZED,
        "a copy of the cookie no longer works"
    );

    let mut said_why = false;
    let mut closed = false;
    for _ in 0..20 {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_next(cx)),
        )
        .await
        .expect("the stream moves");
        match next {
            None => {
                closed = true;
                break;
            }
            Some(chunk) => {
                said_why |= String::from_utf8_lossy(&chunk.unwrap()).contains("session_ended");
            }
        }
    }
    assert!(said_why, "the stream says why it ends");
    assert!(closed, "and then ends");
}

#[actix_web::test]
async fn unknown_key_ids_refetch_the_key_set_at_most_once_a_minute() {
    let idp = idp().await;
    let client = super::oidc::OidcClient::new(oidc_web(&idp).oidc.unwrap());
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("nobody-knows-me".into());
    let now = chrono::Utc::now().timestamp();
    let forged = jsonwebtoken::encode(
        &header,
        &serde_json::json!({"iss": idp.server.uri(), "aud": CLIENT_ID, "sub": "x", "exp": now + 60}),
        &jsonwebtoken::EncodingKey::from_rsa_der(&FORGER.der),
    )
    .unwrap();
    for _ in 0..5 {
        assert!(client.validate(&forged, None).await.is_err());
    }
    let fetches = idp
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/jwks")
        .count();
    assert_eq!(fetches, 2, "the initial fetch and one forced refetch");
}

#[actix_web::test]
async fn a_discovery_document_for_another_issuer_is_refused() {
    let idp = idp().await;
    let mut config = oidc_web(&idp).oidc.unwrap();
    config.endpoint = format!("{}/", idp.server.uri());
    let tolerant = super::oidc::OidcClient::new(config);
    assert!(
        tolerant.discovery().await.is_ok(),
        "a trailing slash is the same issuer"
    );

    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": "https://evil.example",
            "authorization_endpoint": format!("{}/authorize", other.uri()),
            "token_endpoint": format!("{}/token", other.uri()),
            "jwks_uri": format!("{}/jwks", other.uri()),
        })))
        .mount(&other)
        .await;
    let mut config = oidc_web(&idp).oidc.unwrap();
    config.endpoint = other.uri();
    let err = super::oidc::OidcClient::new(config)
        .discovery()
        .await
        .unwrap_err();
    assert!(
        err.description().contains("issuer"),
        "{}",
        err.description()
    );
}

#[actix_web::test]
async fn an_empty_webhook_secret_verifies_nothing() {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"").unwrap();
    mac.update(b"body");
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert!(!crate::source::verify_webhook_signature("", b"body", &sig));
}

// ------------------------------------------------------------ trace links

fn finished_rollout(id: &str, trace_id: Option<&str>) -> crate::store::RolloutRecord {
    crate::store::RolloutRecord {
        id: id.into(),
        revision: "r1".into(),
        state: buckle_api::RolloutState::Completed,
        policy: Default::default(),
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        triggered_by: "controller".into(),
        runtime: crate::rollout::model::RolloutRuntime {
            trace_id: trace_id.map(str::to_owned),
            ..Default::default()
        },
    }
}

#[actix_web::test]
async fn trace_ids_come_with_links_when_a_template_is_configured() {
    let e = env_with(WebConfig::default(), |s| {
        s.trace_url_template = Some("https://ui.example/trace?trace_id={trace_id}".into());
    })
    .await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    let (traced, untraced) = (
        finished_rollout("7", Some("00af7651")),
        finished_rollout("8", None),
    );
    let run = crate::store::RunRecord {
        id: String::new(),
        outcome_json: None,
        host: "pi-1".into(),
        rollout: None,
        revision: "r1".into(),
        plan_digest: "d".into(),
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        outcome: "succeeded".into(),
        changes: 1,
        removals: 0,
        error: None,
        trace_id: Some("feed01".into()),
    };
    e.store
        .call(move |db| {
            db.rollouts().save(&traced)?;
            db.rollouts().save(&untraced)?;
            db.runs().add(&run)
        })
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let get = |uri: &'static str| test::TestRequest::get().uri(uri).to_request();

    let list: buckle_api::Page<buckle_api::RolloutSummary> =
        test::read_body_json(test::call_service(&app, get("/api/v1/rollouts")).await).await;
    let link = |id: &str| {
        list.items
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.trace_url.clone())
    };
    assert_eq!(
        link("7").as_deref(),
        Some("https://ui.example/trace?trace_id=00af7651")
    );
    assert_eq!(link("8"), None, "no trace id, no link");
    let detail: buckle_api::RolloutDetail =
        test::read_body_json(test::call_service(&app, get("/api/v1/rollouts/7")).await).await;
    assert_eq!(
        detail.summary.trace_url.as_deref(),
        Some("https://ui.example/trace?trace_id=00af7651")
    );
    let host: buckle_api::HostDetail =
        test::read_body_json(test::call_service(&app, get("/api/v1/hosts/pi-1")).await).await;
    assert_eq!(
        host.recent_runs[0].trace_url.as_deref(),
        Some("https://ui.example/trace?trace_id=feed01")
    );
    let runs: buckle_api::Page<buckle_api::RunSummary> =
        test::read_body_json(test::call_service(&app, get("/api/v1/hosts/pi-1/runs")).await).await;
    assert_eq!(runs.items[0].trace_url, host.recent_runs[0].trace_url);
    assert_eq!(
        host.latest_run.as_ref().and_then(|r| r.trace_url.clone()),
        host.recent_runs[0].trace_url,
        "the latest run's report links its trace too"
    );

    // The controller publishes rollouts without links; the stream adds them.
    let resp = test::call_service(&app, get("/api/v1/events")).await;
    let mut body = resp.into_body().boxed();
    assert!(next_chunk(&mut body).await.contains("connected"));
    e.state
        .events
        .publish(buckle_api::LiveEvent::RolloutChanged {
            rollout: crate::rollout::view::summary(&finished_rollout("7", Some("00af7651")), &[]),
        });
    let frame = next_chunk(&mut body).await;
    assert!(
        frame.contains(r#""trace_url":"https://ui.example/trace?trace_id=00af7651""#),
        "{frame}"
    );
}

#[actix_web::test]
async fn without_a_template_trace_ids_stay_text() {
    let e = env(WebConfig::default()).await;
    e.store
        .call(|db| db.rollouts().save(&finished_rollout("7", Some("00af7651"))))
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let detail: buckle_api::RolloutDetail = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/rollouts/7")
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(detail.summary.trace_id.as_deref(), Some("00af7651"));
    assert_eq!(detail.summary.trace_url, None);
}

// ------------------------------------------------------------ rollout names

#[actix_web::test]
async fn rollouts_are_named_and_found_by_name() {
    // `twin` shares `clash`'s first 33 bits, so the two share a name.
    let (solo, clash, twin) = (
        "0b5e7d21-4c3a-4e8f-9a1b-2c3d4e5f6a7b",
        "3f2a9c1e-8b4d-4f6a-9c2e-1d7b5a3e9f01",
        "3f2a9c1e-9000-4000-8000-000000000000",
    );
    let e = env(WebConfig::default()).await;
    e.store
        .call(move |db| {
            for id in [solo, clash, twin] {
                db.rollouts().save(&finished_rollout(id, None))?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let get = |uri: String| test::TestRequest::get().uri(&uri).to_request();
    let name = |id: &str| buckle_api::rollout_name(id).unwrap();

    let list: buckle_api::Page<buckle_api::RolloutSummary> =
        test::read_body_json(test::call_service(&app, get("/api/v1/rollouts".into())).await).await;
    assert!(
        list.items
            .iter()
            .all(|r| r.name.as_deref() == Some(name(&r.id).as_str()))
    );

    let typed = name(solo).replace('.', "-").to_uppercase();
    for key in [solo.to_owned(), name(solo), typed] {
        let resp = test::call_service(&app, get(format!("/api/v1/rollouts/{key}"))).await;
        assert_eq!(resp.status(), StatusCode::OK, "{key}");
        let detail: buckle_api::RolloutDetail = test::read_body_json(resp).await;
        assert_eq!(detail.summary.id, solo, "{key}");
    }
    for (uri, status) in [
        (
            format!("/api/v1/rollouts/{}/plans", name(solo)),
            StatusCode::OK,
        ),
        (
            format!("/api/v1/rollouts/{}", name(clash)),
            StatusCode::CONFLICT,
        ),
        (format!("/api/v1/rollouts/{clash}"), StatusCode::OK),
        (
            "/api/v1/rollouts/abandon.ability.able".into(),
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(
            test::call_service(&app, get(uri.clone())).await.status(),
            status,
            "{uri}"
        );
    }
}

// ------------------------------------------------------------ drift

#[actix_web::test]
async fn a_host_serves_the_drift_its_last_report_found() {
    let e = env(WebConfig::default()).await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    let plan = serde_json::to_vec(&crate::store::drift::tests::drift_plan(true)).unwrap();
    e.store
        .call(move |db| db.drift().record("pi-1", true, &plan, chrono::Utc::now()))
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let get = || {
        test::TestRequest::get()
            .uri("/api/v1/hosts/pi-1")
            .to_request()
    };
    let host: buckle_api::HostDetail =
        test::read_body_json(test::call_service(&app, get()).await).await;
    assert!(host.summary.drifted);
    let keys: Vec<&str> = host.drift.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["/etc/app.conf", "vm.swappiness", "/etc/grey"],
        "by module (app, common, grey), kind and key"
    );
    assert!(host.drift.iter().all(|d| d.detected_at.is_some()));
    assert!(
        host.drift[0]
            .unified
            .as_deref()
            .is_some_and(|u| u.contains("+managed"))
    );

    e.store.call(|db| db.drift().clear("pi-1")).await.unwrap();
    let host: buckle_api::HostDetail =
        test::read_body_json(test::call_service(&app, get()).await).await;
    assert!(!host.summary.drifted);
    assert!(host.drift.is_empty());
}

// ------------------------------------------------------------ reject

/// Stores a valid revision `id` whose fleet config is `buckle_yaml`.
async fn revision(e: &Env, id: &str, buckle_yaml: &str) {
    let dir = e.state.revisions.dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("buckle.yaml"), buckle_yaml).unwrap();
    let rec = crate::store::RevisionRecord {
        id: id.into(),
        commit: id.into(),
        author: "a".into(),
        message: format!("revision {id}"),
        committed_at: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        tarball_digest: Some("t".into()),
        status: buckle_api::RevisionStatus::Valid,
        errors: vec![],
        module_count: 0,
        source: "path".into(),
    };
    e.store
        .call(move |db| db.revisions().upsert(&rec))
        .await
        .unwrap();
}

/// A connected host whose agent answers every plan request with `digest`.
fn fake_agent(e: &Env, host: &str, digest: &str) {
    use buckle_proto::v1::{PlanResult, agent_message::Msg as AMsg, server_message::Msg as SMsg};
    let (_, mut rx) = e.sessions.attach(host);
    let (controller, name, digest) = (
        e.state.controller.clone(),
        host.to_owned(),
        digest.to_owned(),
    );
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if let Some(SMsg::Plan(p)) = m.msg {
                controller.agent_message(
                    &name,
                    AMsg::Plan(PlanResult {
                        revision: p.revision,
                        digest: digest.clone(),
                        changes: 1,
                        plan_json: b"{}".to_vec(),
                        ..Default::default()
                    }),
                );
            }
        }
    });
    e.state.controller.connected(crate::rollout::Connected {
        host: host.to_owned(),
        new_session: true,
        ..Default::default()
    });
}

async fn wait_for_state(e: &Env, id: &str, state: buckle_api::RolloutState) {
    for _ in 0..200 {
        let i = id.to_owned();
        let r = e.store.call(move |db| db.rollouts().get(&i)).await.unwrap();
        if r.is_some_and(|r| r.state == state) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("rollout {id} never reached {state:?}");
}

#[actix_web::test]
async fn rejecting_a_rollout_awaiting_approval_aborts_it_with_the_reason_audited() {
    let e = env(WebConfig::default()).await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    revision(
        &e,
        "r1",
        "version: 2\nrollout:\n  phases:\n    - { name: canary, select: 'true', approval: required }\n",
    )
    .await;
    fake_agent(&e, "pi-1", "plan-a");
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    let post = |uri: String, body: serde_json::Value| {
        test::TestRequest::post()
            .uri(&uri)
            .cookie(cookie.clone())
            .insert_header((auth::CSRF_HEADER, token.clone()))
            .set_json(body)
            .to_request()
    };
    let started: buckle_api::RolloutSummary = test::read_body_json(
        test::call_service(
            &app,
            post("/api/v1/revisions/r1/rollout".into(), serde_json::json!({})),
        )
        .await,
    )
    .await;
    let id = started.id;
    wait_for_state(&e, &id, buckle_api::RolloutState::AwaitingApproval).await;

    let reject = |reason: &str| {
        post(
            format!("/api/v1/rollouts/{id}/reject"),
            serde_json::json!({ "reason": reason }),
        )
    };
    let blank = test::call_service(&app, reject("  ")).await;
    assert_eq!(
        blank.status(),
        StatusCode::BAD_REQUEST,
        "a reason is required"
    );

    let resp = test::call_service(&app, reject("the nomad diff rotates the wrong secret")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let detail: buckle_api::RolloutDetail = test::read_body_json(resp).await;
    assert_eq!(detail.summary.state, buckle_api::RolloutState::Aborted);
    let rejected = detail
        .events
        .iter()
        .find(|ev| ev.kind == "rollout.reject")
        .expect("a reject entry on the timeline");
    assert!(
        rejected
            .message
            .contains("the nomad diff rotates the wrong secret")
    );
    assert_eq!(rejected.phase.as_deref(), Some("canary"));

    let page: buckle_api::Page<buckle_api::AuditEntry> = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/audit?filter=action%20%3D%3D%20%22rollout.reject%22")
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(page.total, 1);
    let entry = &page.items[0];
    assert_eq!(entry.subject.as_deref(), Some(id.as_str()));
    assert_eq!(
        entry.detail.as_ref().unwrap()["reason"],
        "the nomad diff rotates the wrong secret"
    );

    let again = test::call_service(&app, reject("twice")).await;
    assert_eq!(
        again.status(),
        StatusCode::CONFLICT,
        "only while awaiting approval"
    );
}

#[actix_web::test]
async fn reject_is_409_unless_awaiting_approval_and_404_for_no_rollout() {
    let e = env(WebConfig::default()).await;
    e.store
        .call(|db| db.rollouts().save(&finished_rollout("7", None)))
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let (cookie, token) = csrf(&app).await;
    for (uri, expected) in [
        ("/api/v1/rollouts/7/reject", StatusCode::CONFLICT),
        ("/api/v1/rollouts/99/reject", StatusCode::NOT_FOUND),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(uri)
                .cookie(cookie.clone())
                .insert_header((auth::CSRF_HEADER, token.clone()))
                .set_json(buckle_api::RejectRequest {
                    reason: "no".into(),
                })
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), expected, "{uri}");
    }
}

// ------------------------------------------------------------ typed plans

#[actix_web::test]
async fn plans_are_served_typed_and_deduplicated_by_digest() {
    let e = env(WebConfig::default()).await;
    let plan = crate::views::tests::review_plan();
    let stored = serde_json::to_value(&plan).unwrap();
    let digest = plan.digest.to_string();
    let host = |name: &str, plan: Option<serde_json::Value>| crate::store::RolloutHostRecord {
        rollout: "7".into(),
        host: name.into(),
        phase: "canary".into(),
        wave: Some(1),
        state: crate::rollout::model::HostPhase::Healthy,
        plan_digest: Some(digest.clone()),
        plan,
        result: None,
        health: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        runtime: crate::rollout::model::HostRuntime {
            plan_changes: Some(4),
            plan_removals: Some(2),
            destructive: true,
            phase_index: Some(0),
            ..Default::default()
        },
    };
    // pi-0 is stored first and holds no copy of the plan.
    let rows = [host("pi-0", None), host("pi-1", Some(stored))];
    e.store
        .call(move |db| {
            db.rollouts().save(&finished_rollout("7", None))?;
            for r in &rows {
                db.rollouts().save_host(r)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let groups: Vec<buckle_api::PlanGroup> = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/rollouts/7/plans")
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(groups.len(), 1, "one digest, one review");
    let g = &groups[0];
    assert_eq!(g.hosts, vec!["pi-0", "pi-1"]);
    assert!(g.destructive);
    let view = g.plan.as_ref().expect("pi-1's copy of the plan");
    assert_eq!(view, &crate::views::plan_view(&plan));
    assert!(
        view.modules[0]
            .steps
            .iter()
            .any(|s| s.destructive && s.key == "/etc/nomad.d/old.hcl")
    );
}

// ------------------------------------------------------------ health gates

#[actix_web::test]
async fn each_phase_names_the_required_checks_of_the_modules_its_hosts_apply() {
    use crate::rollout::model::{HostPhase, HostRuntime, PhaseRuntime};
    use buckle_core::model::{ApprovalPolicy, Parallelism};
    let e = env(WebConfig::default()).await;
    let dir = e.state.revisions.dir("r1");
    for (name, yaml) in [
        (
            "common",
            "name: common\nhealth:\n  - name: unit\n    systemd: { unit: common }\n",
        ),
        (
            "web",
            "name: web\nwhen: 'host.labels.tier == \"web\"'\nhealth:\n  - name: api\n    http: { url: 'http://127.0.0.1/health' }\n  - name: metrics\n    required: false\n    tcp: { port: 9100 }\n",
        ),
        (
            "canary",
            "name: canary\nwhen: 'host.labels.tier == \"canary\"'\nhealth:\n  - name: probe\n    http: { url: 'http://127.0.0.1/probe' }\n",
        ),
    ] {
        std::fs::create_dir_all(dir.join("modules").join(name)).unwrap();
        std::fs::write(dir.join("modules").join(name).join("module.yaml"), yaml).unwrap();
    }
    std::fs::write(dir.join("buckle.yaml"), "version: 2\n").unwrap();
    for (id, tier) in [("pi-1", "canary"), ("pi-2", "web")] {
        add_host(&e.store, id, HostStatus::Active).await;
        let (id, tier) = (id.to_owned(), tier.to_owned());
        e.store
            .call(move |db| {
                db.hosts()
                    .set_labels(&id, &[("tier".to_string(), tier)].into_iter().collect())
                    .map(|_| ())
            })
            .await
            .unwrap();
    }
    // pi-1 has planned: its plan applies `common` only, whatever its labels say.
    let plan = buckle_core::plan::Plan::new(
        buckle_core::plan::RevisionId::new("r1"),
        "pi-1",
        vec![buckle_core::plan::ModulePlan {
            module: "common".into(),
            needs: vec![],
            steps: vec![],
            handlers: vec![],
            provides_facts: vec![],
        }],
        vec![],
        vec![],
    );
    let phase = |name: &str| PhaseRuntime {
        name: name.into(),
        state: buckle_api::PhaseState::Done,
        parallel: Parallelism::Count(1),
        approval: ApprovalPolicy::Auto,
        bake_seconds: 0,
        max_failures: Parallelism::Count(0),
        virtual_stage: false,
        accepted_failures: 0,
        started: 0,
        gate: None,
        bake_until: None,
    };
    let mut rollout = finished_rollout("7", None);
    rollout.runtime.phases = vec![phase("canary"), phase("rest")];
    let host = |name: &str, phase: &str, index: usize, plan: Option<serde_json::Value>| {
        crate::store::RolloutHostRecord {
            rollout: "7".into(),
            host: name.into(),
            phase: phase.into(),
            wave: Some(1),
            state: HostPhase::Healthy,
            plan_digest: Some(format!("d-{name}")),
            plan,
            result: None,
            health: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            runtime: HostRuntime {
                revision: "r1".into(),
                phase_index: Some(index),
                ..Default::default()
            },
        }
    };
    let rows = [
        host(
            "pi-1",
            "canary",
            0,
            Some(serde_json::to_value(&plan).unwrap()),
        ),
        host("pi-2", "rest", 1, None),
    ];
    e.store
        .call(move |db| {
            db.rollouts().save(&rollout)?;
            for r in &rows {
                db.rollouts().save_host(r)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let detail: buckle_api::RolloutDetail = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/rollouts/7")
                .to_request(),
        )
        .await,
    )
    .await;
    let checks = |name: &str| {
        detail
            .phases
            .iter()
            .find(|p| p.name == name)
            .unwrap()
            .health_checks
            .clone()
    };
    assert_eq!(checks("canary"), vec!["common/unit"], "from pi-1's plan");
    assert_eq!(
        checks("rest"),
        vec!["common/unit", "web/api"],
        "from pi-2's labels; informational checks do not gate"
    );
}

// ------------------------------------------------------------ live timeline

async fn audit_row(
    e: &Env,
    action: &str,
    subject: &str,
    detail: serde_json::Value,
) -> buckle_api::AuditEntry {
    let row = crate::store::NewAudit {
        at: chrono::Utc::now(),
        actor: "controller".into(),
        action: action.into(),
        outcome: "ok".into(),
        subject: Some(subject.into()),
        message: format!("{action} of {subject}"),
        detail: Some(detail),
        trace_id: None,
    };
    e.store.call(move |db| db.audit().add(&row)).await.unwrap()
}

async fn next_timeline(
    rx: &mut tokio::sync::broadcast::Receiver<buckle_api::LiveEvent>,
) -> Option<(String, buckle_api::RolloutEvent)> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(buckle_api::LiveEvent::RolloutTimeline { rollout_id, event })) => {
                return Some((rollout_id, event));
            }
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}

#[actix_web::test]
async fn rollout_audit_entries_are_relayed_as_timeline_events() {
    let e = env(WebConfig::default()).await;
    e.store
        .call(|db| db.rollouts().save(&finished_rollout("7", None)))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let relay = super::timeline::spawn(e.store.clone(), e.state.events.clone(), cancel.clone());
    let mut rx = e.state.events.subscribe();
    let publish = |entry: &buckle_api::AuditEntry| {
        e.state.events.publish(buckle_api::LiveEvent::Audit {
            entry: buckle_api::AuditEntryRef {
                id: entry.id,
                at: entry.at,
                action: entry.action.clone(),
                message: entry.message.clone(),
            },
        })
    };

    let paused = audit_row(
        &e,
        "rollout.host.failed",
        "7",
        serde_json::json!({"host": "pi-5", "phase": "home"}),
    )
    .await;
    publish(&paused);
    let (rollout, event) = next_timeline(&mut rx).await.expect("a timeline event");
    assert_eq!(rollout, "7");
    assert_eq!(event.host.as_deref(), Some("pi-5"));
    // Exactly the entry GET /rollouts/7 serves (stored time precision
    // included), so a page that has it already can tell.
    let app = test::init_service(app(e.state.clone())).await;
    let detail: buckle_api::RolloutDetail = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/rollouts/7")
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(detail.events, vec![event]);

    // Not a rollout's timeline: a host's registration, another subject.
    for (action, subject) in [("host.approve", "7"), ("revision.valid", "abc1234")] {
        let row = audit_row(&e, action, subject, serde_json::json!({})).await;
        publish(&row);
        assert_eq!(next_timeline(&mut rx).await, None, "{action} on {subject}");
    }
    cancel.cancel();
    relay.await.unwrap();
}

#[actix_web::test]
async fn a_rollout_timeline_reads_oldest_first() {
    let e = env(WebConfig::default()).await;
    e.store
        .call(|db| db.rollouts().save(&finished_rollout("7", None)))
        .await
        .unwrap();
    for action in ["rollout.started", "rollout.phase.done", "rollout.completed"] {
        audit_row(&e, action, "7", serde_json::json!({})).await;
    }
    let app = test::init_service(app(e.state.clone())).await;
    let detail: buckle_api::RolloutDetail = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/rollouts/7")
                .to_request(),
        )
        .await,
    )
    .await;
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["rollout.started", "rollout.phase.done", "rollout.completed"]
    );
}

// ------------------------------------------------------------ run reports

#[actix_web::test]
async fn run_reports_are_served_per_host_and_per_rollout_host() {
    let e = env(WebConfig::default()).await;
    add_host(&e.store, "pi-1", HostStatus::Active).await;
    let plan = crate::views::tests::review_plan();
    let digest = plan.digest.to_string();
    let row = |name: &str, plan: Option<serde_json::Value>| crate::store::RolloutHostRecord {
        rollout: "7".into(),
        host: name.into(),
        phase: "canary".into(),
        wave: Some(1),
        state: crate::rollout::model::HostPhase::Planned,
        plan_digest: plan.as_ref().map(|_| digest.clone()),
        plan,
        result: None,
        health: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        runtime: crate::rollout::model::HostRuntime {
            revision: "r1".into(),
            phase_index: Some(0),
            ..Default::default()
        },
    };
    let (planned, unplanned) = (
        row("pi-1", Some(serde_json::to_value(&plan).unwrap())),
        row("pi-2", None),
    );
    e.store
        .call(move |db| {
            let mut rollout = finished_rollout("7", None);
            rollout.runtime.phases = vec![crate::rollout::model::PhaseRuntime {
                name: "canary".into(),
                state: buckle_api::PhaseState::Running,
                parallel: buckle_core::model::Parallelism::Count(1),
                approval: buckle_core::model::ApprovalPolicy::Auto,
                bake_seconds: 0,
                max_failures: buckle_core::model::Parallelism::Count(0),
                virtual_stage: false,
                accepted_failures: 0,
                started: 0,
                gate: None,
                bake_until: None,
            }];
            db.rollouts().save(&rollout)?;
            db.rollouts().save_host(&planned)?;
            db.rollouts().save_host(&unplanned)
        })
        .await
        .unwrap();
    let app = test::init_service(app(e.state.clone())).await;
    let get = |uri: String| test::TestRequest::get().uri(&uri).to_request();

    // Before the host starts: the plan, everything pending.
    let pending: buckle_api::RunReport = test::read_body_json(
        test::call_service(&app, get("/api/v1/rollouts/7/hosts/pi-1/report".into())).await,
    )
    .await;
    assert_eq!(pending.state, buckle_api::RunState::Pending);
    assert!(!pending.modules.is_empty());
    assert!(
        pending
            .modules
            .iter()
            .flat_map(|m| &m.steps)
            .all(|s| s.state == buckle_api::StepState::Pending)
    );
    let resp = test::call_service(&app, get("/api/v1/rollouts/7/hosts/pi-2/report".into())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no plan yet");
    let resp = test::call_service(&app, get("/api/v1/rollouts/7/hosts/nope/report".into())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // A run of that plan, in progress.
    let run = crate::store::RunRecord {
        id: String::new(),
        host: "pi-1".into(),
        rollout: Some("7".into()),
        revision: "r1".into(),
        plan_digest: digest.clone(),
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        outcome: crate::store::RUNNING.into(),
        changes: 0,
        removals: 0,
        error: None,
        trace_id: None,
        outcome_json: None,
    };
    let module = plan.modules[0].module.clone();
    let event = serde_json::to_string(&buckle_core::engine::ApplyEvent::ModuleStarted {
        module: module.clone(),
    })
    .unwrap();
    let run_id = e
        .store
        .call(move |db| {
            let id = db.runs().add(&run)?;
            db.runs().record_event(
                &id,
                "module:x",
                None,
                None,
                None,
                &event,
                chrono::Utc::now(),
            )?;
            Ok(id)
        })
        .await
        .unwrap();
    let live: buckle_api::RunReport = test::read_body_json(
        test::call_service(&app, get("/api/v1/rollouts/7/hosts/pi-1/report".into())).await,
    )
    .await;
    assert_eq!(live.id, run_id);
    assert_eq!(live.state, buckle_api::RunState::Running);
    let m = live.modules.iter().find(|m| m.name == module).unwrap();
    assert_eq!(m.state, buckle_api::ModuleState::Running);

    let by_host: buckle_api::RunReport = test::read_body_json(
        test::call_service(&app, get(format!("/api/v1/hosts/pi-1/runs/{run_id}"))).await,
    )
    .await;
    assert_eq!(by_host, live);
    let resp = test::call_service(&app, get("/api/v1/hosts/pi-1/runs/unknown".into())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let detail: buckle_api::HostDetail =
        test::read_body_json(test::call_service(&app, get("/api/v1/hosts/pi-1".into())).await)
            .await;
    assert_eq!(detail.latest_run.map(|r| r.id), Some(run_id.clone()));
    assert_eq!(
        detail.recent_runs[0].id, run_id,
        "the run in progress is listed"
    );
    assert_eq!(
        detail.recent_runs[0].state,
        Some(buckle_api::RunState::Running)
    );
    assert_eq!(
        detail.recent_runs[0].finished_at, detail.recent_runs[0].started_at,
        "a run in progress has no duration yet"
    );
    let runs: buckle_api::Page<buckle_api::RunSummary> = test::read_body_json(
        test::call_service(&app, get("/api/v1/hosts/pi-1/runs?limit=1".into())).await,
    )
    .await;
    assert_eq!(runs.items[0].id, run_id);
    assert_eq!(runs.items[0].state, Some(buckle_api::RunState::Running));
    assert_eq!(runs.total, 1);

    let rollout: buckle_api::RolloutDetail =
        test::read_body_json(test::call_service(&app, get("/api/v1/rollouts/7".into())).await)
            .await;
    let host = rollout
        .phases
        .iter()
        .flat_map(|p| &p.hosts)
        .find(|h| h.host == "pi-1")
        .unwrap();
    assert_eq!(host.run_id.as_deref(), Some(run_id.as_str()));
}
