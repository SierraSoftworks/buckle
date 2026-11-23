//! Local runs: a caller on the agent's socket plans, checks health and reads
//! the status, in turn with what the server asks for. These run in real
//! time, over a real socket, with the client the command line uses.

use super::hardening::{assign_with, gated_with, secret};
use super::*;
use crate::local::client::{Client, Events};
use crate::local::{Choice, Event, Request, Status};
use crate::testing::{Call, plan_with_changes};
use buckle_core::health::{HealthReport, HealthScope};
use buckle_core::plan::RevisionId;
use pretty_assertions::assert_eq;
use rstest::rstest;
use std::time::Duration;

fn rev(id: &str) -> RevisionId {
    RevisionId::new(id)
}

fn local() -> Setup {
    Setup {
        local_socket: true,
        ..Default::default()
    }
}

/// A host whose agent has `rev1` applied and `rev2` assigned by a rollout.
fn in_a_rollout(runner: Arc<FakeRunner>) -> Setup {
    Setup {
        runner,
        prepare: Some(Box::new(|state_dir: &Path| {
            let store = Store::new(state_dir);
            store.ensure().expect("ensure");
            for id in ["rev1", "rev2"] {
                crate::bundle::extract_atomic(
                    &bundle(FLEET_YAML),
                    &store.revision_dir(&rev(id)).expect("dir"),
                )
                .expect("extract");
            }
            store
                .save_agent_state(&crate::store::AgentState {
                    applied_revision: Some(rev("rev1")),
                    assigned_revision: Some(rev("rev2")),
                    ..Default::default()
                })
                .expect("agent state");
        })),
        ..local()
    }
}

async fn eventually<T, F: Future<Output = Option<T>>>(what: &str, mut f: impl FnMut() -> F) -> T {
    for _ in 0..1000 {
        if let Some(value) = f().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened: {what}");
}

/// A client of the agent's socket, once the agent serves it.
async fn connect(h: &Harness) -> Client {
    let socket = h.socket();
    eventually("the agent serves its socket", || async {
        Client::connect(&socket).await.ok()
    })
    .await
}

/// The agent's status once `pred` holds for it.
async fn status_when(client: &mut Client, pred: impl Fn(&Status) -> bool) -> Status {
    for _ in 0..1000 {
        let status = client.status().await.expect("status");
        if pred(&status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent never reached the expected status");
}

fn idle(status: &Status) -> bool {
    status.running.is_none() && status.queued.is_empty()
}

/// The rest of a run's events, `Done` last.
async fn finish(mut events: Events) -> Vec<Event> {
    let mut all = Vec::new();
    while let Some(event) = events.next().await.expect("event") {
        all.push(event);
    }
    assert!(
        matches!(all.last(), Some(Event::Done { .. })),
        "a run ends with Done: {all:?}"
    );
    all
}

fn exit_code(events: &[Event]) -> i32 {
    match events.last() {
        Some(Event::Done {
            exit_code,
            error: None,
        }) => *exit_code,
        other => panic!("the run did not end well: {other:?}"),
    }
}

#[rstest]
#[case::applied(Choice::Applied, "rev1", true, 0)]
#[case::assigned(Choice::Assigned, "rev2", false, 2)]
#[tokio::test]
async fn a_local_plan_is_of_the_revision_the_caller_chose(
    #[case] choice: Choice,
    #[case] revision: &str,
    #[case] drift: bool,
    #[case] exit: i32,
) {
    let fake = FakeRunner::new();
    let newer = plan_with_changes(&rev("rev2"), 2);
    fake.with_plans(&rev("rev2"), vec![newer.clone()]);
    let mut h = Harness::start(in_a_rollout(fake.clone())).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let mut client = connect(&h).await;
    let status = status_when(&mut client, |s| idle(s) && s.connected).await;
    assert_eq!(
        (status.host_id.as_str(), status.applied, status.assigned),
        ("test-host", Some(rev("rev1")), Some(rev("rev2")))
    );

    let select = "module.name == \"local\"".to_owned();
    let request = Request::Plan {
        revision: choice,
        select: Some(select.clone()),
    };
    let events = finish(client.run(request, false).await.expect("accepted")).await;

    assert!(
        matches!(
            &events[0],
            Event::Started { revision: r, applied: Some(a), assigned: Some(b), .. }
                if r.0 == revision && a.0 == "rev1" && b.0 == "rev2"
        ),
        "{events:?}"
    );
    let planned = events.iter().find_map(|e| match e {
        Event::Plan { plan, .. } => Some(plan.changes()),
        _ => None,
    });
    assert_eq!(planned, Some(if exit == 0 { 0 } else { newer.changes() }));
    assert_eq!(exit_code(&events), exit, "0 unchanged, 2 with changes");
    // The applied revision is planned as a drift pass plans it; a newer
    // one as its rollout would.
    assert!(
        fake.calls().iter().any(|c| matches!(
            c,
            Call::Plan { revision: r, select: s, .. } if r.0 == revision && s.as_ref() == Some(&select)
        )),
        "{:?}",
        fake.calls()
    );
    assert_eq!(
        fake.drift_flags().last(),
        Some(&("plan", rev(revision), drift))
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test]
async fn a_local_run_without_a_revision_says_what_to_do() {
    let mut h = Harness::start(local()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let mut client = connect(&h).await;
    let request = Request::Health {
        revision: Choice::Applied,
        modules: Vec::new(),
    };
    let events = finish(client.run(request, false).await.expect("accepted")).await;
    let Some(Event::Done {
        exit_code: 1,
        error: Some(error),
    }) = events.last()
    else {
        panic!("the run is refused: {events:?}");
    };
    assert!(error.message.contains("no revision yet"), "{error:?}");
    assert!(!error.advice.is_empty(), "the caller is told what to do");
    h.stop().await.expect("clean exit");
}

#[tokio::test]
async fn local_runs_take_their_turn_behind_the_agent_s_work() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let (mut h, runner) = gated_with(fake.clone(), local()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;
    let mut client = connect(&h).await;
    let health = || Request::Health {
        revision: Choice::Applied,
        modules: vec!["m".into()],
    };

    // A caller that will not wait is refused while the apply runs.
    let refused = finish(client.run(health(), true).await.expect("accepted")).await;
    assert!(
        matches!(
            refused.as_slice(),
            [Event::Done { exit_code: 1, error: Some(e) }] if e.message.contains("apply rev1")
        ),
        "{refused:?}"
    );

    // One that waits is told what it waits behind.
    let mut waiting = client.run(health(), false).await.expect("accepted");
    let first = waiting.next().await.expect("event");
    assert!(
        matches!(&first, Some(Event::Queued { ahead }) if ahead.len() == 1 && ahead[0].contains("apply rev1")),
        "{first:?}"
    );

    // One that leaves while it waits gives up its place.
    let abandoned = Request::Plan {
        revision: Choice::Applied,
        select: Some("abandoned".into()),
    };
    let leaving = connect(&h)
        .await
        .run(abandoned, false)
        .await
        .expect("accepted");
    let busy = status_when(&mut client, |s| s.queued.len() == 2).await;
    assert!(
        busy.running
            .as_deref()
            .is_some_and(|r| r.contains("apply rev1")),
        "{busy:?}"
    );
    assert!(busy.queued[0].contains("local health by"), "{busy:?}");
    drop(leaving);
    status_when(&mut client, |s| s.queued.len() == 1).await;

    runner.release.notify_one();
    let rest = finish(waiting).await;
    assert!(
        matches!(&rest[0], Event::Started { revision, .. } if revision.0 == "rev1"),
        "the run starts once the apply is done: {rest:?}"
    );
    assert!(rest.iter().any(|e| matches!(e, Event::Health(_))));
    assert_eq!(exit_code(&rest), 0);
    status_when(&mut client, idle).await;
    let calls = fake.calls();
    let at = |pred: &dyn Fn(&Call) -> bool| calls.iter().position(pred);
    let applied = at(&|c| matches!(c, Call::Apply { .. })).expect("applied");
    let checked = at(&|c| {
        matches!(
            c,
            Call::Health { scope: HealthScope::Manual, modules: Some(m), .. } if m == &["m"]
        )
    })
    .expect("the local health checks ran");
    assert!(applied < checked, "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, Call::Plan { select: Some(s), .. } if s == "abandoned")),
        "the abandoned run never ran: {calls:?}"
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test]
async fn a_stopping_agent_ends_the_runs_it_never_started() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let (mut h, runner) = gated_with(fake.clone(), local()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;
    let mut client = connect(&h).await;
    let health = || Request::Health {
        revision: Choice::Applied,
        modules: Vec::new(),
    };
    let mut waiting = client.run(health(), false).await.expect("accepted");
    assert!(matches!(
        waiting.next().await.expect("event"),
        Some(Event::Queued { .. })
    ));

    // The agent finishes its apply before it exits, and takes nothing new.
    h.shutdown.cancel();
    let mut tries = 0;
    loop {
        let refused = finish(client.run(health(), false).await.expect("accepted")).await;
        if matches!(
            refused.as_slice(),
            [Event::Done { error: Some(e), .. }] if e.message.contains("stopping")
        ) {
            break;
        }
        tries += 1;
        assert!(tries < 1000, "new runs are never refused: {refused:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    runner.release.notify_one();
    let mut rest = Vec::new();
    while let Some(event) = waiting.next().await.expect("event") {
        rest.push(event);
    }
    assert!(
        rest.iter().all(|e| matches!(e, Event::Queued { .. })),
        "the run never started, and its caller's stream just ends: {rest:?}"
    );
    h.join().await.expect("clean exit");
    assert!(
        !fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Health { .. })),
        "{:?}",
        fake.calls()
    );
    assert!(!h.socket().exists(), "the socket is removed on the way out");
}

#[tokio::test]
async fn secrets_never_cross_the_socket() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    fake.with_health(|_, scope| HealthReport {
        scope,
        results: vec![buckle_core::health::HealthResult {
            module: "m".into(),
            check: "api".into(),
            kind: "http".into(),
            required: true,
            status: buckle_core::health::HealthStatus::fail(
                "GET https://user:hunter2@api/health returned 401",
            ),
            at: chrono::Utc::now(),
            duration_ms: 1,
        }],
    });
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..local()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session
        .send(assign_with(
            "rev1",
            &digest,
            vec![secret("db", b"hunter2", "01020304")],
        ))
        .await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    session.expect(apply_result).await;

    let mut client = connect(&h).await;
    let request = Request::Health {
        revision: Choice::Applied,
        modules: Vec::new(),
    };
    let events = finish(client.run(request, false).await.expect("accepted")).await;
    let report = events
        .iter()
        .find_map(|e| match e {
            Event::Health(report) => Some(report),
            _ => None,
        })
        .expect("a report");
    let reason = serde_json::to_string(report).expect("json");
    assert!(!reason.contains("hunter2"), "{reason}");
    assert!(reason.contains("401"), "the rest stays: {reason}");
    assert_eq!(exit_code(&events), 1, "a failing required check exits 1");
    h.stop().await.expect("clean exit");
}
