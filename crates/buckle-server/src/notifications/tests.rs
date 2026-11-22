use super::webhooks::{Deliveries, backoff, signature};
use super::*;
use crate::store::NewHost;
use buckle_api::attention::AttentionKind;
use buckle_api::{Connection, HostStatus};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Now, as precisely as the store keeps it.
fn now() -> DateTime<Utc> {
    use chrono::SubsecRound;
    Utc::now().trunc_subsecs(6)
}

struct Fixture {
    store: Store,
    notifier: Notifier,
    events: EventBus,
    outbox: Arc<Notify>,
    _dir: tempfile::TempDir,
}

fn hook(name: &str, url: &str) -> WebhookConfig {
    serde_yaml::from_str(&format!(
        "{{ name: {name}, url: \"{url}\", secret: s3cret }}"
    ))
    .unwrap()
}

async fn fixture(hooks: Vec<WebhookConfig>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::in_memory().await.unwrap();
    let events = EventBus::default();
    let outbox = Arc::new(Notify::new());
    let notifier = Notifier::new(NotifierDeps {
        store: store.clone(),
        revisions: Arc::new(RevisionCache::new(dir.path())),
        events: events.clone(),
        clock: Arc::new(crate::clock::TokioClock::new()),
        hooks,
        base_url: Some("https://buckle.example/".into()),
        expected_version: "2.0.0".into(),
        outbox: outbox.clone(),
    });
    Fixture {
        store,
        notifier,
        events,
        outbox,
        _dir: dir,
    }
}

impl Fixture {
    async fn host(&self, id: &str, status: HostStatus) {
        let id = id.to_owned();
        self.store
            .call(move |db| {
                db.hosts().create(&NewHost {
                    id: id.clone(),
                    status,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: Utc::now(),
                })?;
                db.hosts()
                    .set_connection(&id, Connection::Online, Utc::now())
            })
            .await
            .unwrap();
    }

    async fn queued(&self, hook: &str) -> Vec<serde_json::Value> {
        let hook = hook.to_owned();
        self.store
            .call(move |db| {
                let mut out = Vec::new();
                let mut stmt = db
                    .conn()
                    .prepare("SELECT payload FROM webhook_outbox WHERE hook = ?1 ORDER BY id")
                    .unwrap();
                let rows = stmt.query_map([hook], |r| r.get::<_, String>(0)).unwrap();
                for r in rows {
                    out.push(serde_json::from_str(&r.unwrap()).unwrap());
                }
                Ok(out)
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn each_item_opens_once_and_resolves_once_per_webhook() {
    let f = fixture(vec![
        hook("a", "http://a.invalid"),
        hook("b", "http://b.invalid"),
    ])
    .await;
    let mut live = f.events.subscribe();
    f.host("new-1", HostStatus::Pending).await;
    let t0 = now();

    let items = f.notifier.pass(t0).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, AttentionKind::Registration);
    assert_eq!(items[0].opened_at, Some(t0));
    assert!(matches!(live.try_recv(), Ok(LiveEvent::Attention { list }) if list.items == items));
    // Nothing repeats while it stays open.
    f.notifier.pass(t0 + Duration::seconds(30)).await.unwrap();
    assert!(
        live.try_recv().is_err(),
        "an unchanged list is not announced again"
    );
    for hook in ["a", "b"] {
        let queued = f.queued(hook).await;
        assert_eq!(queued.len(), 1, "{hook}");
        assert_eq!(queued[0]["event"], OPENED);
        assert_eq!(queued[0]["item"]["id"], "host/new-1/registration");
        assert_eq!(queued[0]["url"], "https://buckle.example/hosts/new-1");
    }

    f.store
        .call(|db| db.hosts().set_status("new-1", HostStatus::Active))
        .await
        .unwrap();
    let t1 = t0 + Duration::minutes(1);
    // Missing for less than a minute: still open, nothing resolved.
    assert_eq!(f.notifier.pass(t1).await.unwrap().len(), 1);
    assert_eq!(f.queued("a").await.len(), 1);
    assert!(
        f.notifier
            .pass(t1 + Duration::seconds(61))
            .await
            .unwrap()
            .is_empty()
    );
    let queued = f.queued("a").await;
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[1]["event"], RESOLVED);
    assert_eq!(queued[1]["item"]["id"], "host/new-1/registration");
    assert!(queued[1]["resolved_at"].is_string());
    assert_ne!(queued[0]["delivery"], queued[1]["delivery"]);
}

#[tokio::test]
async fn a_condition_that_returns_quickly_repeats_nothing() {
    let f = fixture(vec![hook("a", "http://a.invalid")]).await;
    f.host("new-1", HostStatus::Pending).await;
    let t0 = now();
    f.notifier.pass(t0).await.unwrap();
    let set = |s| {
        let store = f.store.clone();
        async move {
            store
                .call(move |db| db.hosts().set_status("new-1", s))
                .await
                .unwrap()
        }
    };
    set(HostStatus::Active).await;
    f.notifier.pass(t0 + Duration::seconds(10)).await.unwrap();
    set(HostStatus::Pending).await;
    f.notifier.pass(t0 + Duration::seconds(20)).await.unwrap();
    set(HostStatus::Active).await;
    f.notifier.pass(t0 + Duration::seconds(30)).await.unwrap();
    assert_eq!(
        f.queued("a").await.len(),
        1,
        "the absence restarts when the item returns"
    );
}

#[tokio::test]
async fn open_items_survive_a_restart_without_repeating() {
    let f = fixture(vec![hook("a", "http://a.invalid")]).await;
    f.host("new-1", HostStatus::Pending).await;
    let t0 = now();
    f.notifier.pass(t0).await.unwrap();
    let again = Notifier::new(NotifierDeps {
        store: f.store.clone(),
        revisions: Arc::new(RevisionCache::new(f._dir.path())),
        events: EventBus::default(),
        clock: Arc::new(crate::clock::TokioClock::new()),
        hooks: vec![hook("a", "http://a.invalid")],
        base_url: None,
        expected_version: "2.0.0".into(),
        outbox: f.outbox.clone(),
    });
    let items = again.pass(t0 + Duration::minutes(5)).await.unwrap();
    assert_eq!(items[0].opened_at, Some(t0), "it opened before the restart");
    assert_eq!(f.queued("a").await.len(), 1);
}

#[tokio::test]
async fn identity_refusals_are_listed_until_a_grant() {
    let f = fixture(vec![]).await;
    let audit = |action: &str, at| crate::store::NewAudit {
        at,
        actor: "pi-1".into(),
        action: action.into(),
        outcome: String::new(),
        subject: Some("home-nomad".into()),
        message: "pi-1 was refused a deploy identity for home-nomad: not an executor".into(),
        detail: Some(serde_json::json!({ "purpose": "deploy" })),
        trace_id: None,
    };
    let t0 = now();
    let e = audit("identity.denied", t0);
    f.store.call(move |db| db.audit().add(&e)).await.unwrap();
    let items = f.notifier.pass(t0).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, AttentionKind::IdentityRefused);
    assert!(
        items[0].summary.ends_with("not an executor"),
        "{}",
        items[0].summary
    );
    let e = audit("identity.issued", t0 + Duration::seconds(1));
    f.store.call(move |db| db.audit().add(&e)).await.unwrap();
    let later = t0 + Duration::minutes(5);
    f.notifier.pass(later).await.unwrap();
    assert!(
        f.notifier
            .pass(later + Duration::minutes(2))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deliveries_are_signed_ordered_and_retried() {
    let server = MockServer::start().await;
    let f = fixture(vec![hook("a", &format!("{}/hook", server.uri()))]).await;
    let t0 = now();
    f.store
        .call(move |db| {
            let n = db.notifications();
            n.enqueue("a", "d1", OPENED, r#"{"n":1}"#, t0)?;
            n.enqueue("a", "d2", RESOLVED, r#"{"n":2}"#, t0)
        })
        .await
        .unwrap();
    let d = Deliveries::new(
        f.store.clone(),
        vec![hook("a", &format!("{}/hook", server.uri()))],
        Arc::new(crate::clock::TokioClock::new()),
        f.outbox.clone(),
    );

    // Refused first: the head waits, the next event is not sent around it.
    let refuse = Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let next = d.pass(t0).await.unwrap();
    assert_eq!(next, Some(t0 + backoff(1)));
    drop(refuse);
    let status = f
        .store
        .call(|db| db.notifications().hook_status("a"))
        .await
        .unwrap();
    assert_eq!(status.failing_since, Some(t0));
    assert_eq!(status.queued, 2);
    assert!(status.last_error.unwrap().contains("503"));
    assert_eq!(
        d.pass(t0 + Duration::seconds(1)).await.unwrap(),
        Some(t0 + backoff(1)),
        "not due yet"
    );

    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("content-type", "application/json"))
        .and(header_exists("x-buckle-delivery"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;
    assert_eq!(d.pass(t0 + backoff(1)).await.unwrap(), None);
    let got: Vec<Request> = server.received_requests().await.unwrap();
    let delivered: Vec<_> = got.iter().skip(1).collect();
    let deliveries: Vec<_> = delivered
        .iter()
        .map(|r| {
            r.headers
                .get("x-buckle-delivery")
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        deliveries,
        vec!["d1", "d2"],
        "in order, the retried one first"
    );
    for r in &delivered {
        let sig = r
            .headers
            .get("x-hub-signature-256")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(sig, signature("s3cret", &r.body));
    }
    let event = delivered[0].headers.get("x-buckle-event").unwrap();
    assert_eq!(event, OPENED);
    let status = f
        .store
        .call(|db| db.notifications().hook_status("a"))
        .await
        .unwrap();
    assert_eq!((status.failing_since, status.queued), (None, 0));
}

#[tokio::test]
async fn an_event_is_given_up_on_after_a_day() {
    let f = fixture(vec![]).await;
    let t0 = now();
    f.store
        .call(move |db| db.notifications().enqueue("a", "d1", OPENED, "{}", t0))
        .await
        .unwrap();
    // Nothing listens there; the event is dropped without an attempt.
    let d = Deliveries::new(
        f.store.clone(),
        vec![hook("a", "http://127.0.0.1:9")],
        Arc::new(crate::clock::TokioClock::new()),
        f.outbox.clone(),
    );
    assert_eq!(d.pass(t0 + Duration::hours(25)).await.unwrap(), None);
    assert!(f.queued("a").await.is_empty());
}

#[tokio::test]
async fn a_webhook_that_keeps_failing_needs_attention() {
    let f = fixture(vec![hook("a", "http://a.invalid")]).await;
    let t0 = now();
    f.store
        .call(move |db| {
            db.notifications()
                .hook_failed("a", t0, "connection refused")
        })
        .await
        .unwrap();
    assert!(
        f.notifier
            .pass(t0 + Duration::minutes(1))
            .await
            .unwrap()
            .is_empty()
    );
    let items = f.notifier.pass(t0 + Duration::minutes(6)).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, AttentionKind::WebhookFailing);
    assert!(items[0].summary.contains("connection refused"));
    // Its own opening is queued for it too, to arrive once it recovers.
    assert_eq!(f.queued("a").await.len(), 1);
}

#[test]
fn backoff_doubles_up_to_an_hour() {
    let delays: Vec<i64> = [1, 2, 3, 10, 40].map(|n| backoff(n).num_seconds()).to_vec();
    assert_eq!(delays, vec![15, 30, 60, 3600, 3600]);
}
