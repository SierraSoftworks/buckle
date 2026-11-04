use super::*;
use rstest::rstest;
use std::collections::BTreeMap;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADA: &str = "ada@example.com";

fn now() -> DateTime<Utc> {
    "2026-11-04T10:00:00Z".parse().unwrap()
}

fn settings() -> JoinSettings {
    JoinSettings {
        server_url: "http://buckle.example.ts.net:7700".into(),
        insecure_plaintext: true,
        ca_fingerprint: None,
        install_url: "https://example.com/v2.5.0".into(),
        version: Some("2.5.0".into()),
        ttl: Duration::from_secs(3600),
        methods: vec![JoinMethod::Tailscale, JoinMethod::Token],
        approval_required: false,
        tailscale: None,
    }
}

fn provisioning(api: &str) -> Provisioning {
    Provisioning {
        config: TailscaleProvisioning {
            flow: ProvisioningFlow::App,
            client_id: "client-1".into(),
            client_secret: "s3cret".into(),
            tags: vec![],
            node_attributes: vec!["custom:buckle".into()],
            api_url: api.into(),
            login_url: "https://login.example.com".into(),
        },
        tags: vec!["tag:packmule".into(), "tag:trailhead".into()],
        redirect_uri: "https://buckle.example.ts.net/api/v1/join/tailscale/callback".into(),
    }
}

fn request(platform: JoinPlatform, method: JoinMethod) -> JoinRequest {
    JoinRequest {
        platform,
        method,
        host_id: Some("web-1".into()),
        labels: BTreeMap::from([("role".into(), "web server".into())]),
        tailscale_tags: vec![],
    }
}

fn command(ticket: &JoinTicket) -> &str {
    match &ticket.state {
        JoinState::Ready { command } => command,
        other => panic!("expected a ready ticket, got {other:?}"),
    }
}

/// The value following `flag` in a rendered command, unquoted.
fn value_after<'a>(command: &'a str, flag: &str) -> &'a str {
    let rest = command.split(flag).nth(1).expect(flag).trim_start();
    let end = rest.find([' ', '\n']).unwrap_or(rest.len());
    rest[..end].trim_matches('\'')
}

#[rstest]
#[case::linux(JoinPlatform::Linux, "install.sh | sudo sh", "--tailscale-auth-key")]
#[case::macos(JoinPlatform::Macos, "install.sh | sudo sh", "--tailscale-auth-key")]
#[case::windows(JoinPlatform::Windows, "install.ps1", "-TailscaleAuthKey")]
fn commands_install_join_and_enrol_in_one_paste(
    #[case] platform: JoinPlatform,
    #[case] installer: &str,
    #[case] ts_flag: &str,
) {
    let mut s = settings();
    s.ca_fingerprint = Some("sha256:abc".into());
    let cmd = render(
        &s,
        &request(platform, JoinMethod::Tailscale),
        &["tag:packmule".into()],
        Keys {
            join_token: Some("jt"),
            tailscale_key: Some("tskey-1"),
        },
    );
    assert!(cmd.contains("https://example.com/v2.5.0/"), "{cmd}");
    assert!(cmd.contains(installer), "{cmd}");
    assert_eq!(value_after(&cmd, ts_flag), "tskey-1");
    assert!(cmd.contains("tag:packmule"), "{cmd}");
    assert_eq!(
        value_after(&cmd, "--server"),
        "http://buckle.example.ts.net:7700"
    );
    assert_eq!(value_after(&cmd, "--token"), "jt");
    assert_eq!(value_after(&cmd, "--host-id"), "web-1");
    assert_eq!(value_after(&cmd, "--ca-fingerprint"), "sha256:abc");
    assert!(cmd.contains("--insecure-plaintext"));
    // The label's space is quoted rather than splitting the argument.
    assert!(cmd.contains("'role=web server'"), "{cmd}");
    // Tailscale is joined before Buckle runs.
    assert!(cmd.find(ts_flag) < cmd.find(" up "), "{cmd}");
    // Every line but the last continues the one command.
    let cont = if platform == JoinPlatform::Windows {
        '`'
    } else {
        '\\'
    };
    let lines: Vec<&str> = cmd.lines().collect();
    assert!(
        lines[..lines.len() - 1].iter().all(|l| l.ends_with(cont)),
        "{cmd}"
    );
    assert!(!lines[lines.len() - 1].ends_with(cont), "{cmd}");
}

#[test]
fn commands_without_keys_carry_no_key_flags() {
    let cmd = render(
        &settings(),
        &request(JoinPlatform::Linux, JoinMethod::Tailscale),
        &[],
        Keys::default(),
    );
    assert!(
        !cmd.contains("--token") && !cmd.contains("tailscale"),
        "{cmd}"
    );
}

#[rstest]
#[case::plain("abc", "abc", "abc")]
#[case::space("a b", "'a b'", "'a b'")]
#[case::quote("it's", r"'it'\''s'", "'it''s'")]
#[case::dollar("$HOME", "'$HOME'", "'$HOME'")]
#[case::dash("-x", "-x", "'-x'")]
fn values_are_quoted_for_their_shell(#[case] raw: &str, #[case] sh: &str, #[case] ps: &str) {
    assert_eq!(sh_quote(raw), sh);
    assert_eq!(ps_quote(raw), ps);
}

#[rstest]
#[case::from_base_url(
    Some("https://buckle.example.ts.net"),
    "0.0.0.0:7700",
    TlsMode::None,
    "http://buckle.example.ts.net:7700"
)]
#[case::tls(
    Some("https://buckle.example.com/"),
    "[::]:7800",
    TlsMode::Auto,
    "https://buckle.example.com:7800"
)]
#[case::listen_address(None, "100.64.0.10:7700", TlsMode::None, "http://100.64.0.10:7700")]
fn the_server_url_defaults_to_the_web_host_and_grpc_port(
    #[case] base: Option<&str>,
    #[case] grpc: &str,
    #[case] tls: TlsMode,
    #[case] expected: &str,
) {
    assert_eq!(derive_server_url(base, grpc, tls), expected);
}

#[tokio::test]
async fn a_join_token_enrols_one_host_once_before_it_expires() {
    let joins = Joins::new(settings());
    let ticket = joins
        .create(request(JoinPlatform::Linux, JoinMethod::Token), ADA, now())
        .await
        .unwrap();
    let token = value_after(command(&ticket), "--token").to_owned();

    assert_eq!(joins.redeem(&token, "web-2", now()), None, "another host");
    let later = ticket.expires_at + chrono::Duration::seconds(1);
    assert_eq!(joins.redeem(&token, "web-1", later), None, "expired");
    let redeemed = joins.redeem(&token, "web-1", now()).expect("redeems");
    assert_eq!(redeemed.created_by, ADA);
    assert_eq!(joins.redeem(&token, "web-1", now()), None, "used");
    joins.release(&redeemed.ticket);
    assert!(joins.redeem(&token, "web-1", now()).is_some(), "released");
}

#[rstest]
#[case::auto(false, None)]
#[case::approve(true, Some(()))]
#[tokio::test]
async fn tailnet_joins_carry_a_token_only_to_skip_approval(
    #[case] approval: bool,
    #[case] token: Option<()>,
) {
    let mut s = settings();
    s.approval_required = approval;
    let joins = Joins::new(s);
    let ticket = joins
        .create(
            request(JoinPlatform::Linux, JoinMethod::Tailscale),
            ADA,
            now(),
        )
        .await
        .unwrap();
    assert_eq!(command(&ticket).contains("--token").then_some(()), token);
}

#[tokio::test]
async fn tickets_are_private_to_their_creator() {
    let joins = Joins::new(settings());
    let ticket = joins
        .create(request(JoinPlatform::Linux, JoinMethod::Token), ADA, now())
        .await
        .unwrap();
    assert!(joins.get(&ticket.id, ADA, now()).is_ok());
    assert_eq!(
        joins.get(&ticket.id, "eve@example.com", now()),
        Err(JoinError::NotFound)
    );
}

#[rstest]
#[case::method(JoinMethod::Token, None, &[], vec![JoinMethod::Tailscale])]
#[case::host(JoinMethod::Token, Some("web 1"), &[], vec![JoinMethod::Token])]
#[case::tags_without_provisioning(JoinMethod::Tailscale, None, &["tag:packmule"], vec![JoinMethod::Tailscale])]
#[tokio::test]
async fn invalid_requests_are_refused(
    #[case] method: JoinMethod,
    #[case] host: Option<&str>,
    #[case] tags: &[&str],
    #[case] methods: Vec<JoinMethod>,
) {
    let mut s = settings();
    s.methods = methods;
    let joins = Joins::new(s);
    let mut r = request(JoinPlatform::Linux, method);
    r.host_id = host.map(str::to_owned);
    r.tailscale_tags = tags.iter().map(|t| t.to_string()).collect();
    assert!(matches!(
        joins.create(r, ADA, now()).await,
        Err(JoinError::Invalid(_))
    ));
}

#[tokio::test]
async fn live_tickets_are_capped() {
    let joins = Joins::new(settings());
    for _ in 0..MAX_TICKETS {
        joins
            .create(request(JoinPlatform::Linux, JoinMethod::Token), ADA, now())
            .await
            .unwrap();
    }
    let r = request(JoinPlatform::Linux, JoinMethod::Token);
    assert_eq!(
        joins.create(r.clone(), ADA, now()).await,
        Err(JoinError::TooMany)
    );
    // Expired tickets make room.
    let later = now() + chrono::Duration::hours(2);
    assert!(joins.create(r, ADA, later).await.is_ok());
}

fn authorize_state(ticket: &JoinTicket) -> String {
    let JoinState::Authorize { authorize_url } = &ticket.state else {
        panic!("expected consent, got {:?}", ticket.state);
    };
    let url = url::Url::parse(authorize_url).unwrap();
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/a/oauth_authorize");
    assert_eq!(q["client_id"], "client-1");
    assert_eq!(q["scope"], TAILSCALE_SCOPE);
    assert_eq!(q["response_type"], "code");
    assert!(q["redirect_uri"].ends_with(CALLBACK_PATH));
    q["state"].clone()
}

async fn provisioned(tailscale: &MockServer) -> Joins {
    let mut s = settings();
    s.tailscale = Some(provisioning(&tailscale.uri()));
    Joins::new(s)
}

#[tokio::test]
async fn consent_trades_the_code_for_a_single_use_auth_key() {
    let tailscale = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=c0de"))
        .and(body_string_contains("client_secret=s3cret"))
        .and(body_string_contains("node_attributes=custom%3Abuckle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tskey-auth-xyz",
            "token_type": "Bearer",
            "expires_in": 600,
        })))
        .expect(1)
        .mount(&tailscale)
        .await;
    let joins = provisioned(&tailscale).await;
    let mut r = request(JoinPlatform::Linux, JoinMethod::Tailscale);
    r.tailscale_tags = vec!["trailhead".into()];
    let ticket = joins.create(r, ADA, now()).await.unwrap();
    let state = authorize_state(&ticket);

    // Only the admin who asked may complete it.
    assert_eq!(
        joins
            .complete_tailscale(&state, Some("c0de"), None, "eve@example.com", now())
            .await,
        Err(JoinError::NotFound)
    );
    let id = joins
        .complete_tailscale(&state, Some("c0de"), None, ADA, now())
        .await
        .unwrap();
    let ready = joins.get(&id, ADA, now()).unwrap();
    let cmd = command(&ready);
    assert_eq!(value_after(cmd, "--tailscale-auth-key"), "tskey-auth-xyz");
    assert_eq!(value_after(cmd, "--tailscale-tags"), "tag:trailhead");
    assert!(
        !cmd.contains("s3cret"),
        "the client secret stays on the server"
    );
    // The key's own lifetime caps the ticket's.
    assert_eq!(ready.expires_at, now() + chrono::Duration::seconds(600));
    // A replayed callback finds nothing.
    assert_eq!(
        joins
            .complete_tailscale(&state, Some("c0de"), None, ADA, now())
            .await,
        Err(JoinError::NotFound)
    );
}

#[rstest]
#[case::denied(None, Some("access_denied"), 200, "denied")]
#[case::refused(Some("bad"), None, 400, "refused")]
#[tokio::test]
async fn failed_consent_leaves_a_failed_ticket(
    #[case] code: Option<&str>,
    #[case] error: Option<&str>,
    #[case] status: u16,
    #[case] reason: &str,
) {
    let tailscale = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/oauth/token"))
        .respond_with(ResponseTemplate::new(status).set_body_string("invalid_grant"))
        .mount(&tailscale)
        .await;
    let joins = provisioned(&tailscale).await;
    let ticket = joins
        .create(
            request(JoinPlatform::Windows, JoinMethod::Tailscale),
            ADA,
            now(),
        )
        .await
        .unwrap();
    let state = authorize_state(&ticket);
    let id = joins
        .complete_tailscale(&state, code, error, ADA, now())
        .await
        .unwrap();
    match joins.get(&id, ADA, now()).unwrap().state {
        JoinState::Failed { reason: r } => assert!(r.contains(reason), "{r}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn only_offered_tags_may_be_advertised() {
    let tailscale = MockServer::start().await;
    let joins = provisioned(&tailscale).await;
    let mut r = request(JoinPlatform::Linux, JoinMethod::Tailscale);
    r.tailscale_tags = vec!["tag:admin".into()];
    assert!(matches!(
        joins.create(r, ADA, now()).await,
        Err(JoinError::Invalid(_))
    ));
}

async fn minting(tailscale: &MockServer) -> Joins {
    let mut s = settings();
    let mut p = provisioning(&tailscale.uri());
    p.config.flow = ProvisioningFlow::Client;
    s.tailscale = Some(p);
    Joins::new(s)
}

#[tokio::test]
async fn the_client_flow_mints_a_tagged_key_without_consent() {
    let tailscale = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/oauth/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("client_secret=s3cret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "api-token",
            "token_type": "Bearer",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&tailscale)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/tailnet/-/keys"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer api-token",
        ))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "capabilities": { "devices": { "create": {
                "reusable": false,
                "preauthorized": true,
                "tags": ["tag:trailhead"],
            } } },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "k1",
            "key": "tskey-auth-minted",
        })))
        .expect(1)
        .mount(&tailscale)
        .await;
    let joins = minting(&tailscale).await;
    assert_eq!(
        joins.settings().options().tailscale.map(|t| t.consent),
        Some(false)
    );
    let mut r = request(JoinPlatform::Linux, JoinMethod::Tailscale);
    r.tailscale_tags = vec!["tag:trailhead".into()];
    let ticket = joins.create(r, ADA, now()).await.unwrap();
    let cmd = command(&ticket);
    assert_eq!(
        value_after(cmd, "--tailscale-auth-key"),
        "tskey-auth-minted"
    );
    assert!(
        !cmd.contains("s3cret") && !cmd.contains("api-token"),
        "{cmd}"
    );
}

#[tokio::test]
async fn a_refused_mint_leaves_a_failed_ticket() {
    let tailscale = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
        .mount(&tailscale)
        .await;
    let joins = minting(&tailscale).await;
    let ticket = joins
        .create(
            request(JoinPlatform::Linux, JoinMethod::Tailscale),
            ADA,
            now(),
        )
        .await
        .unwrap();
    match ticket.state {
        JoinState::Failed { reason } => assert!(reason.contains("refused"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}
