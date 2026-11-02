use super::fake_server::ServerSession;
use super::*;
use crate::testing::{Call, health, plan_with_changes};
use buckle_core::engine::ApplyEvent;
use buckle_core::health::HealthScope;
use buckle_core::plan::{Plan, RevisionId};
use pretty_assertions::assert_eq;
use rstest::rstest;
use std::time::Duration;
use tokio::time::Instant;

fn rev(id: &str) -> RevisionId {
    RevisionId::new(id)
}

/// Assign, plan and apply `id` (planned as `plan`) on an open session.
async fn roll_out(
    h: &Harness,
    session: &mut ServerSession,
    id: &str,
    plan: &Plan,
) -> pb::ApplyResult {
    let digest = h.fleet.add_revision(id, bundle(FLEET_YAML));
    session.send(assign(id, &digest)).await;
    session.send(request_plan(id)).await;
    let planned = session.expect(plan_result).await;
    assert_eq!(planned.digest, plan.digest.to_string());
    session
        .send(pb::server_message::Msg::Apply(apply(id, &planned.digest)))
        .await;
    session.expect(apply_result).await
}

#[tokio::test]
async fn enrolment_persists_the_issued_token_and_uses_it_next_time() {
    let mut h = Harness::start(Setup {
        yaml: "bootstrap_token: boot-strap\n".into(),
        tcp: true,
        ..Default::default()
    })
    .await;

    let mut first = h.sessions.accept().await;
    assert_eq!(first.token.as_deref(), Some("boot-strap"));
    assert_eq!(first.hello.token, "boot-strap");
    assert!(first.hello.bootstrap);
    assert_eq!(first.hello.host_id, "test-host");
    assert_eq!(first.hello.agent_version, "2.0.0-test");
    assert_eq!(
        first.hello.labels.get("role").map(String::as_str),
        Some("worker")
    );
    assert_eq!(first.hello.platform, std::env::consts::OS);

    first.send(welcome(None, Some("per-host-token"))).await;
    // Facts are sent after the Welcome is processed (and the token stored).
    first
        .expect(|m| matches!(m, pb::agent_message::Msg::Facts(_)).then_some(()))
        .await;
    let token_path = h.state_dir().join("token");
    assert_eq!(
        std::fs::read_to_string(&token_path).expect("token"),
        "per-host-token"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    first.close();

    let second = h.sessions.accept().await;
    assert_eq!(second.token.as_deref(), Some("per-host-token"));
    assert_eq!(second.hello.token, "per-host-token");
    assert!(!second.hello.bootstrap);
    assert_eq!(h.stop().await.expect("clean exit"), 0);
}

#[tokio::test(start_paused = true)]
async fn assign_plan_apply_reports_the_plan_coalesced_events_and_the_result() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 3);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut events = vec![ApplyEvent::ModuleStarted { module: "m".into() }];
    for i in 0..40 {
        events.push(ApplyEvent::StepStarted {
            module: "m".into(),
            task_id: format!("t{i}"),
            kind: "file.copy".into(),
        });
        events.push(ApplyEvent::StepFinished {
            module: "m".into(),
            task_id: format!("t{i}"),
            kind: "file.copy".into(),
            changed: true,
            summary: "wrote".into(),
        });
    }
    runner.with_events(events);
    let mut h = Harness::start(Setup {
        runner,
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;

    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    let planned = session.expect(plan_result).await;
    assert_eq!(planned.revision, "rev1");
    assert_eq!(planned.digest, plan.digest.to_string());
    assert_eq!(planned.changes, 3);
    assert_eq!(planned.error, "");
    let decoded: Plan = serde_json::from_slice(&planned.plan_json).expect("plan json");
    assert_eq!(decoded, plan);
    assert!(h.state_dir().join("revisions/rev1/buckle.yaml").is_file());

    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            &planned.digest,
        )))
        .await;
    let received = session
        .collect_until(|m| matches!(m, pb::agent_message::Msg::Result(_)))
        .await;
    let event_times: Vec<Instant> = received
        .iter()
        .filter(|(_, m)| matches!(m, pb::agent_message::Msg::Apply(_)))
        .map(|(at, _)| *at)
        .collect();
    assert!(!event_times.is_empty());
    assert!(
        event_times.len() < 81,
        "events are coalesced: {}",
        event_times.len()
    );
    for pair in event_times.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(99),
            "at most 10 events per second"
        );
    }
    let first_event = received
        .iter()
        .find_map(|(_, m)| match m {
            pb::agent_message::Msg::Apply(e) => Some(e.clone()),
            _ => None,
        })
        .expect("an event");
    assert_eq!(first_event.revision, "rev1");
    assert_eq!(first_event.plan_digest, plan.digest.to_string());
    let event: ApplyEvent = serde_json::from_slice(&first_event.event_json).expect("event json");
    assert!(matches!(event, ApplyEvent::ModuleStarted { .. }));

    let result = received
        .iter()
        .find_map(|(_, m)| apply_result(m))
        .expect("result");
    assert!(result.succeeded);
    assert_eq!(result.revision, "rev1");
    assert_eq!(result.plan_digest, plan.digest.to_string());
    assert_eq!(result.changes, 3);
    assert_eq!(result.applied_revision, "rev1");
    assert!(result.failed_module.is_empty());
    let outcome: buckle_core::engine::ApplyOutcome =
        serde_json::from_slice(&result.outcome_json).expect("outcome json");
    assert!(outcome.succeeded);

    let state = Store::new(h.state_dir()).load_agent_state();
    assert_eq!(state.applied_revision, Some(rev("rev1")));

    // The next Hello carries the applied revision and the last plan digest.
    session.close();
    let next = h.sessions.accept().await;
    assert_eq!(next.hello.applied_revision, "rev1");
    assert_eq!(next.hello.last_plan_digest, plan.digest.to_string());
    h.stop().await.expect("clean exit");
}

#[rstest]
#[case::without_force(false)]
#[case::force_does_not_bypass_the_digest(true)]
#[tokio::test(start_paused = true)]
async fn a_stale_plan_is_refused(#[case] force: bool) {
    let runner = FakeRunner::new();
    let approved = plan_with_changes(&rev("rev1"), 1);
    let fresh = plan_with_changes(&rev("rev1"), 2);
    runner.with_plans(&rev("rev1"), vec![approved.clone(), fresh.clone()]);
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    assert_eq!(
        session.expect(plan_result).await.digest,
        approved.digest.to_string()
    );

    session
        .send(pb::server_message::Msg::Apply(pb::Apply {
            force,
            ..apply("rev1", approved.digest.as_str())
        }))
        .await;
    let refused = session.expect(plan_result).await;
    assert!(refused.error.starts_with("stale plan"), "{}", refused.error);
    assert_eq!(refused.digest, fresh.digest.to_string());
    assert!(runner.applies().is_empty(), "nothing was applied");
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn waived_checks_and_force_reach_the_runner() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(pb::Apply {
            waived_checks: vec!["m/api".into()],
            force: true,
            ..apply("rev1", plan.digest.as_str())
        }))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    assert_eq!(
        runner.applies(),
        vec![Call::Apply {
            revision: rev("rev1"),
            plan_digest: plan.digest.to_string(),
            waived_checks: vec!["m/api".into()],
            force: true,
        }]
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn bake_health_repeats_until_the_deadline_then_stops() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    roll_out(&h, &mut session, "rev1", &plan).await;

    let until = chrono::Utc::now() + chrono::Duration::seconds(120);
    session
        .send(pb::server_message::Msg::Health(pb::RunHealth {
            request_id: "bake-1".into(),
            scope: pb::HealthScope::Bake as i32,
            modules: vec!["m".into()],
            until: Some(crate::transport::convert::timestamp(until)),
            traceparent: String::new(),
        }))
        .await;
    let mut reports = Vec::new();
    for _ in 0..4 {
        let report = session.expect(health_report).await;
        reports.push((Instant::now(), report));
    }
    for (_, report) in &reports {
        assert_eq!(report.request_id, "bake-1");
        assert_eq!(report.scope, pb::HealthScope::Bake as i32);
        assert_eq!(report.revision, "rev1");
        assert!(report.results.iter().all(|r| r.passed));
    }
    assert!(reports[1].0 - reports[0].0 >= Duration::from_secs(30));

    // Well past the deadline nothing more is checked.
    tokio::time::sleep(Duration::from_secs(600)).await;
    let bake_runs = runner
        .calls()
        .into_iter()
        .filter(|c| {
            matches!(
                c,
                Call::Health {
                    scope: HealthScope::Bake,
                    ..
                }
            )
        })
        .count();
    assert_eq!(bake_runs, 4);
    assert!(
        runner.calls().iter().any(
            |c| matches!(c, Call::Health { modules: Some(m), .. } if m == &vec!["m".to_string()])
        ),
        "modules are passed through"
    );
    h.stop().await.expect("clean exit");
}

#[rstest]
#[case::corrects(true)]
#[case::reports_only(false)]
#[tokio::test(start_paused = true)]
async fn drift_is_reported_and_corrected_when_configured(#[case] correct: bool) {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 2);
    let drift = plan_with_changes(&rev("rev1"), 1);
    // RequestPlan, the apply's re-plan, then every drift pass.
    runner.with_plans(
        &rev("rev1"),
        vec![plan.clone(), plan.clone(), drift.clone()],
    );
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        yaml: format!("drift: {{ interval: 10m, correct: {correct} }}\n"),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    roll_out(&h, &mut session, "rev1", &plan).await;

    let reported = session
        .expect(|m| match m {
            pb::agent_message::Msg::Drift(d) => Some(d.clone()),
            _ => None,
        })
        .await;
    assert_eq!(reported.revision, "rev1");
    assert_eq!(reported.plan_digest, drift.digest.to_string());
    assert_eq!(reported.changes, 1);
    assert_eq!(reported.corrected, correct);
    let report = session.expect(health_report).await;
    assert_eq!(report.scope, pb::HealthScope::Drift as i32);
    assert_eq!(report.request_id, "");
    assert_eq!(runner.applies().len(), if correct { 2 } else { 1 });
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn no_drift_sends_an_empty_drift_report_with_health() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 2);
    let settled = plan_with_changes(&rev("rev1"), 0);
    runner.with_plans(&rev("rev1"), vec![plan.clone(), plan.clone(), settled]);
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        yaml: "drift: { interval: 10m }\n".into(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    roll_out(&h, &mut session, "rev1", &plan).await;
    let reported = session
        .expect(|m| match m {
            pb::agent_message::Msg::Drift(d) => Some(d.clone()),
            _ => None,
        })
        .await;
    assert_eq!(reported.plan_digest, "");
    assert!(!reported.corrected);
    assert_eq!(
        session.expect(health_report).await.scope,
        pb::HealthScope::Drift as i32
    );
    assert_eq!(runner.applies().len(), 1);
    h.stop().await.expect("clean exit");
}

fn stable_applies(runner: &FakeRunner) -> usize {
    runner
        .applies()
        .into_iter()
        .filter(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1"))
        .count()
}

/// Welcome with a stable revision, then roll out rev1 (whose health is
/// `rev1_healthy`), then the server goes away.
async fn apply_then_lose_the_server(rev1_healthy: bool) -> (Harness, Plan) {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    runner.with_plans(&rev("stable1"), vec![plan_with_changes(&rev("stable1"), 1)]);
    runner.with_health(move |revision, scope| health(scope, revision.0 != "rev1" || rev1_healthy));
    let mut h = Harness::start(Setup {
        runner,
        ..Default::default()
    })
    .await;
    h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    let mut session = h.sessions.accept().await;
    session.send(welcome(Some("stable1"), None)).await;
    let result = roll_out(&h, &mut session, "rev1", &plan).await;
    assert!(result.succeeded);
    assert!(
        h.state_dir()
            .join("revisions/stable1/buckle.yaml")
            .is_file(),
        "the stable revision is cached"
    );
    h.fleet.set_unreachable(true);
    session.close();
    (h, plan)
}

#[tokio::test(start_paused = true)]
async fn local_rollback_triggers_when_cut_off_with_failing_health() {
    let (mut h, _) = apply_then_lose_the_server(false).await;
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(3600),
        h.runner.wait_for(|calls| {
            calls
                .iter()
                .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1"))
        }),
    )
    .await
    .expect("rolled back");
    let elapsed = Instant::now() - started;
    assert!(
        elapsed >= Duration::from_secs(299),
        "not before the window closes: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(400),
        "soon after it closes: {elapsed:?}"
    );

    // Reported once a session is back.
    h.fleet.set_unreachable(false);
    let mut session = h.sessions.accept().await;
    assert_eq!(session.hello.applied_revision, "stable1");
    session.send(welcome(Some("stable1"), None)).await;
    let rollback = session
        .expect(|m| match m {
            pb::agent_message::Msg::Rollback(r) => Some(r.clone()),
            _ => None,
        })
        .await;
    assert_eq!(rollback.from_revision, "rev1");
    assert_eq!(rollback.to_revision, "stable1");
    assert!(rollback.reason.contains("m/api"), "{}", rollback.reason);
    assert!(!rollback.evidence.is_empty());
    assert!(rollback.evidence.iter().all(|e| !e.passed && e.required));
    assert!(
        Store::new(h.state_dir())
            .load_agent_state()
            .pending_rollbacks
            .is_empty()
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn local_rollback_does_not_trigger_when_health_is_fine() {
    let (h, _) = apply_then_lose_the_server(true).await;
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert_eq!(stable_applies(&h.runner), 0);
    let state = Store::new(h.state_dir()).load_agent_state();
    assert_eq!(
        state.watchdog, None,
        "the window passed and the watchdog disarmed"
    );
    assert!(state.pending_rollbacks.is_empty());
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn local_rollback_does_not_trigger_when_the_server_was_already_gone() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    runner.with_plans(&rev("stable1"), vec![plan_with_changes(&rev("stable1"), 1)]);
    runner.with_health(|revision, scope| health(scope, revision.0 != "rev1"));
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(Some("stable1"), None)).await;
    // The host runs the stable revision before the rollout reaches it.
    roll_out(
        &h,
        &mut session,
        "stable1",
        &plan_with_changes(&rev("stable1"), 1),
    )
    .await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    session.expect(plan_result).await;
    // A bake on the previous state keeps the worker busy; the approved apply
    // queues behind it, and the session dies before the apply starts.
    // The bake outlasts the session, so only the session ending stops it.
    let until = chrono::Utc::now() + chrono::Duration::seconds(3000);
    session
        .send(pb::server_message::Msg::Health(pb::RunHealth {
            request_id: "b".into(),
            scope: pb::HealthScope::Baseline as i32,
            until: Some(crate::transport::convert::timestamp(until)),
            ..Default::default()
        }))
        .await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    session.expect(health_report).await;
    h.fleet.set_unreachable(true);
    session.close();

    tokio::time::timeout(
        Duration::from_secs(3600),
        runner.wait_for(|calls| {
            calls
                .iter()
                .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "rev1"))
        }),
    )
    .await
    .expect("the approved apply ran");
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert_eq!(
        stable_applies(&runner),
        1,
        "only the initial rollout of the stable revision"
    );
    assert_eq!(Store::new(h.state_dir()).load_agent_state().watchdog, None);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_restart_mid_window_resumes_the_watchdog() {
    let runner = FakeRunner::new();
    runner.with_plans(&rev("stable1"), vec![plan_with_changes(&rev("stable1"), 1)]);
    runner.with_health(|revision, scope| health(scope, revision.0 != "rev1"));
    let h = Harness::start(Setup {
        runner: runner.clone(),
        prepare: Some(Box::new(|state_dir: &Path| {
            let store = Store::new(state_dir);
            store.ensure().expect("ensure");
            for id in ["rev1", "stable1"] {
                crate::bundle::extract_atomic(
                    &bundle(FLEET_YAML),
                    &store.revision_dir(&rev(id)).expect("dir"),
                )
                .expect("extract");
            }
            // Bookkeeping as an older agent kept it, in the engine's state:
            // adopted on start.
            {
                use buckle_core::state::StateStore;
                store
                    .state_store()
                    .save(&buckle_core::state::State {
                        applied_revision: Some(rev("rev1")),
                        stable_revision: Some(rev("stable1")),
                        ..Default::default()
                    })
                    .expect("state");
            }
            store
                .save_agent_state(&crate::store::AgentState {
                    watchdog: Some(crate::watchdog::WatchdogState {
                        from: rev("rev1"),
                        to: rev("stable1"),
                        deadline_at: chrono::Utc::now() + chrono::Duration::seconds(180),
                        after: Duration::from_secs(300),
                        on_health: true,
                        settle: Duration::from_secs(60),
                        waived_checks: vec![],
                    }),
                    ..Default::default()
                })
                .expect("agent state");
        })),
        ..Default::default()
    })
    .await;
    h.fleet.set_unreachable(true);
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(3600),
        runner.wait_for(|calls| {
            calls
                .iter()
                .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1"))
        }),
    )
    .await
    .expect("rolled back after the resumed window");
    let elapsed = Instant::now() - started;
    assert!(
        elapsed >= Duration::from_secs(179) && elapsed < Duration::from_secs(300),
        "{elapsed:?}"
    );
    let pending = Store::new(h.state_dir())
        .load_agent_state()
        .pending_rollbacks;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].to, rev("stable1"));
    assert!(pending[0].succeeded);
    h.join_after_shutdown().await;
}

impl Harness {
    async fn join_after_shutdown(mut self) {
        self.shutdown.cancel();
        self.join().await.expect("clean exit");
    }
}

#[tokio::test(start_paused = true)]
async fn reconnect_backoff_grows_and_resets_after_a_session() {
    let (fleet_probe, mut h) = {
        let h = Harness::start(Setup::default()).await;
        (h.fleet.clone(), h)
    };
    fleet_probe.set_unreachable(true);
    while fleet_probe.attempts().len() < 6 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let attempts = fleet_probe.attempts();
    let gaps: Vec<u64> = attempts
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs())
        .collect();
    assert_eq!(&gaps[gaps.len() - 4..], &[2, 4, 8, 16], "{gaps:?}");

    fleet_probe.set_unreachable(false);
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session
        .expect(|m| matches!(m, pb::agent_message::Msg::Facts(_)).then_some(()))
        .await;
    let closed_at = Instant::now();
    session.close();
    let _next = h.sessions.accept().await;
    let gap = Instant::now() - closed_at;
    assert!(
        gap <= Duration::from_millis(1100),
        "backoff reset to 1 s, got {gap:?}"
    );
    h.stop().await.expect("clean exit");
}

#[rstest]
#[case::final_rejection(false)]
#[case::retryable(true)]
#[tokio::test(start_paused = true)]
async fn rejection_stops_the_agent_unless_retry_is_allowed(#[case] retry: bool) {
    let mut h = Harness::start(Setup::default()).await;
    let session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Rejected(pb::Rejected {
            reason: "unknown host".into(),
            retry,
        }))
        .await;
    if retry {
        let again = h.sessions.accept().await;
        assert_eq!(again.hello.host_id, "test-host");
        h.stop().await.expect("clean exit");
    } else {
        let err = h.join().await.expect_err("the agent gives up");
        assert!(err.description().contains("unknown host"));
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(h.fleet.attempts().len(), 1, "no further attempts");
    }
}

#[tokio::test(start_paused = true)]
async fn a_corrupted_revision_stream_is_rejected_then_refetched() {
    let mut h = Harness::start(Setup::default()).await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    h.fleet.set_corrupt(true);
    session.send(assign("rev1", &digest)).await;
    let failed = session.expect(plan_result).await;
    assert_eq!(failed.revision, "rev1");
    assert!(failed.error.contains("corrupt"), "{}", failed.error);
    assert!(!h.state_dir().join("revisions/rev1").exists());

    h.fleet.set_corrupt(false);
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    let planned = session.expect(plan_result).await;
    assert_eq!(planned.error, "");
    assert!(h.state_dir().join("revisions/rev1/buckle.yaml").is_file());
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn welcome_and_assign_secrets_are_cached_privately_with_server_fingerprints() {
    let runner = FakeRunner::new();
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session
        .send(pb::server_message::Msg::Assign(pb::Assign {
            secrets: Some(pb::SecretBundle {
                secrets: vec![pb::SecretValue {
                    name: "db".into(),
                    value: b"hunter2".to_vec(),
                    fingerprint: "0a1b2c3d".into(),
                }],
            }),
            ..match assign("rev1", &digest) {
                pb::server_message::Msg::Assign(a) => a,
                _ => unreachable!(),
            }
        }))
        .await;
    session.send(request_plan("rev1")).await;
    session.expect(plan_result).await;
    let store = Store::new(h.state_dir());
    let key = store.install_key().expect("key");
    let cached = store
        .load_secrets(&rev("rev1"), &key)
        .expect("load")
        .expect("cached");
    assert_eq!(
        cached.get("db").expect("db").fingerprint().as_hex(),
        "0a1b2c3d"
    );
    let raw = std::fs::read(h.state_dir().join("secrets/rev1.cbor")).expect("read");
    assert!(!format!("{:?}", runner.calls()).contains("hunter2"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(h.state_dir().join("secrets/rev1.cbor"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(!raw.is_empty());
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn upgrade_installs_the_version_and_exits_zero() {
    let mut h = Harness::start(Setup::default()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session
        .send(pb::server_message::Msg::Upgrade(pb::Upgrade {
            version: "2.1.0".into(),
            when_idle: true,
        }))
        .await;
    assert_eq!(h.join().await.expect("exits"), 0);
    assert_eq!(
        *h.updater.requested.lock().expect("lock"),
        vec![Some("2.1.0".to_string())]
    );
}

#[tokio::test(start_paused = true)]
async fn heartbeats_report_liveness() {
    let mut h = Harness::start(Setup::default()).await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let beat = session
        .expect(|m| match m {
            pb::agent_message::Msg::Heartbeat(b) => Some(b.clone()),
            _ => None,
        })
        .await;
    assert!(!beat.busy);
    assert!(beat.at.is_some());
    h.stop().await.expect("clean exit");
}

#[tokio::test]
async fn local_mode_applies_on_directory_change() {
    let source = tempfile::tempdir().expect("tempdir");
    std::fs::write(source.path().join("buckle.yaml"), "version: 2\n").expect("write");
    let state = tempfile::tempdir().expect("tempdir");
    let config: AgentConfig = serde_yaml::from_str(&format!(
        "source: {}\nhost_id: local-host\nstate_dir: {}\n",
        source.path().display(),
        state.path().display()
    ))
    .expect("config");
    let runner = FakeRunner::new();
    let transport = Arc::new(
        crate::transport::local::LocalTransport::new(source.path())
            .with_debounce(Duration::from_millis(200)),
    );
    let agent = Agent::new(
        config,
        transport,
        runner.clone(),
        Arc::new(FakeUpdater::default()),
        AgentOptions::default(),
    )
    .expect("agent");
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(agent.run(shutdown.clone()));

    let applied_ids = |calls: &[Call]| -> Vec<String> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Apply { revision, .. } => Some(revision.0.clone()),
                _ => None,
            })
            .collect()
    };
    let first = crate::bundle::tree_digest(source.path()).expect("digest");
    tokio::time::timeout(
        Duration::from_secs(20),
        runner.wait_for(|c| !applied_ids(c).is_empty()),
    )
    .await
    .expect("applied at start");
    assert_eq!(applied_ids(&runner.calls()), vec![first.clone()]);

    std::fs::create_dir_all(source.path().join("modules/motd")).expect("mkdir");
    std::fs::write(
        source.path().join("modules/motd/module.yaml"),
        "name: motd\n",
    )
    .expect("write");
    let second = crate::bundle::tree_digest(source.path()).expect("digest");
    assert_ne!(first, second);
    tokio::time::timeout(
        Duration::from_secs(20),
        runner.wait_for(|c| applied_ids(c).len() >= 2),
    )
    .await
    .expect("applied after the change");
    assert_eq!(applied_ids(&runner.calls()), vec![first, second]);
    shutdown.cancel();
    assert_eq!(handle.await.expect("join").expect("clean exit"), 0);
}

#[tokio::test(start_paused = true)]
async fn changed_facts_top_up_secrets_for_newly_selected_modules() {
    let runner = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    runner.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut h = Harness::start(Setup {
        runner: runner.clone(),
        ..Default::default()
    })
    .await;
    h.fleet.set_secrets(vec![pb::SecretValue {
        name: "db".into(),
        value: b"hunter2".to_vec(),
        fingerprint: "01020304".into(),
    }]);
    let yaml = format!("{FLEET_YAML}secrets:\n  db: {{ file: /secrets/db }}\n");
    let digest = h.fleet.add_revision("rev1", bundle(&yaml));
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    session.expect(plan_result).await;
    let store = Store::new(h.state_dir());
    let key = store.install_key().expect("key");
    assert!(
        store
            .load_secrets(&rev("rev1"), &key)
            .expect("load")
            .is_none()
    );

    let mut facts = buckle_core::facts::Facts::new();
    facts.mount(
        "os",
        serde_json::json!({ "family": "debian" }),
        chrono::Utc::now(),
    );
    runner.with_facts(facts);
    // The next facts pass (10 min) sees the change, reports it and tops up.
    session
        .expect(|m| match m {
            pb::agent_message::Msg::Facts(f) => {
                let tree: serde_json::Value = serde_json::from_slice(&f.json).ok()?;
                (tree["os"]["family"] == "debian").then_some(())
            }
            _ => None,
        })
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let cached = store
        .load_secrets(&rev("rev1"), &key)
        .expect("load")
        .expect("topped up");
    assert_eq!(
        cached.get("db").expect("db").fingerprint().as_hex(),
        "01020304"
    );
    h.stop().await.expect("clean exit");
}
