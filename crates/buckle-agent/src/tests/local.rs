//! Local runs: a caller on the agent's socket plans, applies, checks health
//! and reads the status, in turn with what the server asks for. These run in real
//! time, over a real socket, with the client the command line uses.

use super::hardening::{assign_with, gated_with, secret};
use super::*;
use crate::local::client::{Client, Events};
use crate::local::{Choice, Event, Request, Status};
use crate::testing::{Call, plan_with_changes};
use buckle_core::engine::ApplyEvent;
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

fn local_apply() -> Request {
    Request::Apply {
        select: None,
        waive: Vec::new(),
        include_rolled_back: false,
        reboot: false,
    }
}

fn drift_report(msg: &pb::agent_message::Msg) -> Option<pb::Drift> {
    match msg {
        pb::agent_message::Msg::Drift(d) => Some(d.clone()),
        _ => None,
    }
}

/// A host on `rev1` (with `rev2` assigned) whose drift pass reports and
/// corrects nothing on its own.
fn reporting_only(runner: Arc<FakeRunner>) -> Setup {
    Setup {
        yaml: "drift: { interval: 1h, correct: false }\n".into(),
        ..in_a_rollout(runner)
    }
}

#[rstest]
#[case::drifted(2)]
#[case::as_configured(0)]
#[tokio::test]
async fn a_local_apply_puts_drift_right_and_the_server_hears_who_ran_it(#[case] changes: usize) {
    let fake = FakeRunner::new();
    let drifted = plan_with_changes(&rev("rev1"), changes);
    fake.with_plans(&rev("rev1"), vec![drifted.clone()]);
    fake.with_events(vec![ApplyEvent::ModuleStarted { module: "m".into() }]);
    let mut h = Harness::start(reporting_only(fake.clone())).await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    // The agent's own pass at start-up reports the drift and leaves it.
    let found = session.expect(drift_report).await;
    assert_eq!((found.changes as usize, found.corrected), (changes, false));
    let mut client = connect(&h).await;
    status_when(&mut client, |s| idle(s) && s.connected).await;

    let events = finish(client.run(local_apply(), false).await.expect("accepted")).await;

    let Event::Started {
        run_id, revision, ..
    } = &events[0]
    else {
        panic!("the run starts: {events:?}");
    };
    assert_eq!(revision, &rev("rev1"), "never the assigned revision");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Plan { plan, .. } if plan.changes() == changes)),
        "the caller sees what will change: {events:?}"
    );
    assert_eq!(exit_code(&events), 0);
    let outcome = events.iter().find_map(|e| match e {
        Event::Outcome { outcome, rebooting } => Some((outcome.succeeded, *rebooting)),
        _ => None,
    });
    if changes == 0 {
        assert_eq!(outcome, None, "nothing to apply: {events:?}");
        let checked = session.expect(drift_report).await;
        assert_eq!((checked.changes, checked.corrected), (0, false));
        assert!(fake.applies().is_empty(), "{:?}", fake.calls());
    } else {
        assert_eq!(outcome, Some((true, false)));
        assert!(
            events.iter().any(|e| matches!(e, Event::Apply(_))),
            "the caller follows the apply: {events:?}"
        );
        // The server hears every part of the run with who ran it.
        let received = session
            .collect_until(|m| matches!(m, pb::agent_message::Msg::Drift(_)))
            .await;
        let origins: Vec<pb::LocalOrigin> = received
            .iter()
            .filter_map(|(_, m)| match m {
                pb::agent_message::Msg::Apply(e) => Some(e.local.clone()),
                pb::agent_message::Msg::Result(r) => Some(r.local.clone()),
                _ => None,
            })
            .map(|origin| origin.expect("a local run names who ran it"))
            .collect();
        assert!(origins.len() >= 2, "events and the result: {received:?}");
        for origin in &origins {
            assert_eq!(&origin.run_id, run_id);
            assert!(!origin.user.is_empty(), "{origin:?}");
        }
        let result = received.iter().find_map(|(_, m)| apply_result(m));
        let result = result.expect("the result is reported");
        assert!(result.succeeded && result.report_id != 0, "{result:?}");
        assert_eq!(result.revision, "rev1");
        let corrected = received.iter().find_map(|(_, m)| drift_report(m));
        assert!(corrected.is_some_and(|d| d.corrected), "{received:?}");
        assert_eq!(
            fake.drift_flags().last(),
            Some(&("apply", rev("rev1"), true)),
            "applied as a drift pass applies: removals never run"
        );
    }
    let after = status_when(&mut client, idle).await;
    assert_eq!(
        (after.applied, after.assigned),
        (Some(rev("rev1")), Some(rev("rev2"))),
        "a local apply never moves the host to another revision"
    );
    h.stop().await.expect("clean exit");
}

#[rstest]
#[case::the_fleet_says_no("active", Some(true), &[], "does not allow")]
#[case::a_server_too_old_to_record_it("active", None, &[], "has not said")]
#[case::a_paused_host("paused", Some(false), &[], "paused")]
#[case::rolled_back_modules("active", Some(false), &["m"], "--include-rolled-back")]
#[tokio::test]
async fn a_local_apply_is_refused_where_the_host_may_not_change(
    #[case] status: &str,
    #[case] apply_off: Option<bool>,
    #[case] rolled_back: &'static [&'static str],
    #[case] why: &str,
) {
    let fake = FakeRunner::new();
    fake.with_plans(&rev("rev1"), vec![plan_with_changes(&rev("rev1"), 1)]);
    let mut setup = reporting_only(fake.clone());
    setup.prepare = Some(Box::new(move |state_dir: &Path| {
        let store = Store::new(state_dir);
        store.ensure().expect("ensure");
        crate::bundle::extract_atomic(
            &bundle(FLEET_YAML),
            &store.revision_dir(&rev("rev1")).expect("dir"),
        )
        .expect("extract");
        store
            .save_agent_state(&crate::store::AgentState {
                applied_revision: Some(rev("rev1")),
                rolled_back_modules: rolled_back.iter().map(|m| (*m).to_owned()).collect(),
                ..Default::default()
            })
            .expect("agent state");
    }));
    let mut h = Harness::start(setup).await;
    let session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            server_version: "2.0.0".into(),
            session_id: "s1".into(),
            status: status.into(),
            local_runs: apply_off.map(|apply_off| pb::LocalRuns { apply_off }),
            ..Default::default()
        }))
        .await;
    let mut client = connect(&h).await;
    status_when(&mut client, |s| idle(s) && s.connected).await;

    let events = finish(client.run(local_apply(), false).await.expect("accepted")).await;

    let Some(Event::Done {
        exit_code: 1,
        error: Some(error),
    }) = events.last()
    else {
        panic!("the apply is refused: {events:?}");
    };
    let said = format!("{} {}", error.message, error.advice.join(" "));
    assert!(said.contains(why), "{said}");
    assert!(fake.applies().is_empty(), "{:?}", fake.calls());
    h.stop().await.expect("clean exit");
}

#[tokio::test]
async fn a_local_result_the_server_never_acknowledged_is_announced_and_sent_again() {
    let fake = FakeRunner::new();
    fake.with_plans(&rev("rev1"), vec![plan_with_changes(&rev("rev1"), 1)]);
    let mut h = Harness::start(reporting_only(fake.clone())).await;
    let mut first = h.sessions.accept().await;
    first.send(welcome(None, None)).await;
    let mut client = connect(&h).await;
    status_when(&mut client, |s| idle(s) && s.connected).await;
    let events = finish(client.run(local_apply(), false).await.expect("accepted")).await;
    assert_eq!(exit_code(&events), 0);
    let sent = first.expect(apply_result).await;
    let run_id = sent.local.clone().expect("a local run").run_id;

    first.close();
    let mut second = h.sessions.accept().await;
    assert_eq!(second.hello.unreported_local_runs, vec![run_id.clone()]);
    assert_eq!(
        second.hello.unreported_apply_revisions,
        Vec::<String>::new(),
        "no rollout's apply of this revision is running or unreported"
    );
    second.send(welcome(None, None)).await;
    let again = second.expect(apply_result).await;
    assert_eq!(
        (again.report_id, again.local.map(|o| o.run_id)),
        (sent.report_id, Some(run_id))
    );
    h.stop().await.expect("clean exit");
}
