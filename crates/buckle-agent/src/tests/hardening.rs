//! Scenarios for the design-conformance hardening: single writer of the
//! engine state, durable reports, session-bound applies, bake rollback,
//! rejection handling and the rest.

use super::*;
use crate::runner::{RunContext, Runner};
use crate::testing::{Call, plan_with_changes};
use async_trait::async_trait;
use buckle_core::engine::{ApplyEvent, ApplyOutcome, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::{HealthReport, HealthScope};
use buckle_core::plan::{Plan, RevisionId};
use buckle_core::state::{FileStateStore, State, StateStore};
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

fn rev(id: &str) -> RevisionId {
    RevisionId::new(id)
}

/// A runner that writes `state.cbor` during apply the way the engine does
/// (load at the start, save at the end), pausing in between until released.
struct StateWritingRunner {
    inner: Arc<FakeRunner>,
    state: FileStateStore,
    started: Notify,
    release: Notify,
}

#[async_trait]
impl Runner for StateWritingRunner {
    async fn facts(&self, previous: &Facts) -> buckle_core::Result<Facts> {
        self.inner.facts(previous).await
    }

    async fn plan_action(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        module: &str,
        action: &str,
    ) -> buckle_core::Result<(Plan, Facts)> {
        self.inner.plan_action(revision, ctx, module, action).await
    }

    async fn run_action(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        module: &str,
        action: &str,
        plan: &Plan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> buckle_core::Result<ApplyOutcome> {
        self.inner
            .run_action(revision, ctx, module, action, plan, events)
            .await
    }

    async fn plan(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
    ) -> buckle_core::Result<(Plan, Facts)> {
        self.inner.plan(revision, ctx).await
    }

    async fn apply(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> buckle_core::Result<ApplyOutcome> {
        let mut state = self.state.load()?;
        self.started.notify_one();
        self.release.notified().await;
        state.applied_revision = Some(revision.id.clone());
        state.facts.mount(
            "engine",
            serde_json::json!({ "marker": revision.id.0 }),
            chrono::Utc::now(),
        );
        self.state.save(&state)?;
        self.inner.apply(revision, ctx, plan, events).await
    }

    async fn health(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> buckle_core::Result<HealthReport> {
        self.inner.health(revision, ctx, scope, modules).await
    }
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..3600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("the condition never held");
}

#[tokio::test(start_paused = true)]
async fn a_welcome_during_an_apply_keeps_the_stable_revision_and_the_engine_state() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let slot: Arc<std::sync::Mutex<Option<Arc<StateWritingRunner>>>> = Arc::default();
    let (inner, built) = (fake.clone(), slot.clone());
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        custom_runner: Some(Box::new(move |dir: &Path| {
            let runner = Arc::new(StateWritingRunner {
                inner,
                state: FileStateStore::new(dir.join("state.cbor")),
                started: Notify::new(),
                release: Notify::new(),
            });
            *built.lock().expect("lock") = Some(runner.clone());
            runner
        })),
        ..Default::default()
    })
    .await;
    let runner = slot.lock().expect("lock").clone().expect("built");

    let mut first = h.sessions.accept().await;
    first.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    first.send(assign("rev1", &digest)).await;
    first
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;

    // The session drops mid-apply and the next one names a stable revision.
    first.close();
    let mut second = h.sessions.accept().await;
    assert_eq!(
        second.hello.unreported_apply_revisions,
        vec!["rev1".to_string()],
        "the running apply is announced so the server waits for its result"
    );
    second.send(welcome(Some("stable1"), None)).await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.load_agent_state().stable_revision == Some(rev("stable1"))).await;

    runner.release.notify_one();
    let result = second.expect(apply_result).await;
    assert!(result.succeeded, "{}", result.error);
    // Let the queued stable assignment and facts pass run too.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let agent = store.load_agent_state();
    assert_eq!(agent.stable_revision, Some(rev("stable1")));
    assert_eq!(agent.applied_revision, Some(rev("rev1")));
    // state.cbor is exactly what the engine saved: the agent never wrote it.
    let engine_state: State = store.load_state().expect("state");
    assert_eq!(engine_state.applied_revision, Some(rev("rev1")));
    assert_eq!(engine_state.stable_revision, None);
    assert_eq!(engine_state.facts.tree["engine"]["marker"], "rev1");
    assert!(
        !h.state_dir().join("state.tmp").exists(),
        "no temp file left behind"
    );
    assert_eq!(fake.applies().len(), 1);
    h.stop().await.expect("clean exit");
}

/// A harness whose runner pauses every apply until released.
async fn gated(fake: Arc<FakeRunner>) -> (Harness, Arc<StateWritingRunner>) {
    let slot: Arc<std::sync::Mutex<Option<Arc<StateWritingRunner>>>> = Arc::default();
    let (inner, built) = (fake.clone(), slot.clone());
    let h = Harness::start(Setup {
        runner: fake,
        custom_runner: Some(Box::new(move |dir: &Path| {
            let runner = Arc::new(StateWritingRunner {
                inner,
                state: FileStateStore::new(dir.join("state.cbor")),
                started: Notify::new(),
                release: Notify::new(),
            });
            *built.lock().expect("lock") = Some(runner.clone());
            runner
        })),
        ..Default::default()
    })
    .await;
    let runner = slot.lock().expect("lock").clone().expect("built");
    (h, runner)
}

#[tokio::test(start_paused = true)]
async fn an_apply_result_without_a_session_is_delivered_on_the_next_one_until_acked() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let (mut h, runner) = gated(fake.clone()).await;

    let mut first = h.sessions.accept().await;
    first.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    first.send(assign("rev1", &digest)).await;
    first
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;
    // The server goes away mid-apply; the result has nowhere to go.
    h.fleet.set_unreachable(true);
    first.close();
    tokio::time::sleep(Duration::from_secs(1)).await;
    runner.release.notify_one();
    let store = Store::new(h.state_dir());
    wait_until(|| !store.load_agent_state().unacked_reports.is_empty()).await;

    // The next session gets it after Welcome.
    h.fleet.set_unreachable(false);
    let mut second = h.sessions.accept().await;
    assert_eq!(second.hello.applied_revision, "rev1");
    assert_eq!(
        second.hello.unreported_apply_revisions,
        vec!["rev1".to_string()]
    );
    second.send(welcome(None, None)).await;
    let result = second.expect(apply_result).await;
    assert!(result.succeeded);
    assert_eq!(result.revision, "rev1");
    assert!(result.report_id > 0);

    // Not acknowledged: the session after that gets the same report again.
    second.close();
    let mut third = h.sessions.accept().await;
    third.send(welcome(None, None)).await;
    let again = third.expect(apply_result).await;
    assert_eq!(
        again.report_id, result.report_id,
        "same id, so the server can dedupe"
    );
    third
        .send(pb::server_message::Msg::Ack(pb::Ack {
            report_ids: vec![again.report_id],
        }))
        .await;
    wait_until(|| store.load_agent_state().unacked_reports.is_empty()).await;

    // Acknowledged: never sent again.
    third.close();
    let mut fourth = h.sessions.accept().await;
    fourth.send(welcome(None, None)).await;
    let until_heartbeat = fourth
        .collect_until(|m| matches!(m, pb::agent_message::Msg::Heartbeat(_)))
        .await;
    assert!(
        until_heartbeat
            .iter()
            .all(|(_, m)| apply_result(m).is_none()),
        "an acknowledged result is not re-sent"
    );
    assert_eq!(fake.applies().len(), 1);
    h.stop().await.expect("clean exit");
}

#[test]
fn local_rollbacks_from_older_agents_become_durable_reports() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("agent.json"),
        serde_json::to_vec(&serde_json::json!({
            "pending_rollbacks": [{
                "from": "rev1", "to": "stable1", "reason": "cut off",
                "modules": [], "evidence": [], "at": "2026-01-01T00:00:00Z",
                "succeeded": true
            }]
        }))
        .expect("json"),
    )
    .expect("write");
    let mut state = Store::new(dir.path()).load_agent_state();
    state.migrate_legacy();
    assert!(state.pending_rollbacks.is_empty());
    assert_eq!(state.unacked_reports.len(), 1);
    let id = state.unacked_reports[0].report_id();
    assert!(id > 0);
    assert!(state.next_report_id() > id, "ids keep increasing");
}

/// A bundle with `buckle.yaml` and one module `m` declaring `health`.
fn bundle_with_module(yaml: &str, module_yaml: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("buckle.yaml"), yaml).expect("write");
    std::fs::create_dir_all(dir.path().join("modules/m")).expect("mkdir");
    std::fs::write(dir.path().join("modules/m/module.yaml"), module_yaml).expect("write");
    crate::bundle::pack_dir(dir.path()).expect("pack")
}

fn rollback_msg(m: &pb::agent_message::Msg) -> Option<pb::LocalRollback> {
    match m {
        pb::agent_message::Msg::Rollback(r) => Some(r.clone()),
        _ => None,
    }
}

fn bake_request(id: &str, seconds: i64) -> pb::server_message::Msg {
    let until = chrono::Utc::now() + chrono::Duration::seconds(seconds);
    pb::server_message::Msg::Health(pb::RunHealth {
        request_id: id.into(),
        scope: pb::HealthScope::Bake as i32,
        until: Some(crate::transport::convert::timestamp(until)),
        ..Default::default()
    })
}

/// Welcome naming `stable1` (cached), then roll out rev1.
async fn rolled_out(fake: Arc<FakeRunner>) -> (Harness, ServerSessionAlias) {
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    fake.with_plans(&rev("stable1"), vec![plan_with_changes(&rev("stable1"), 1)]);
    let mut h = Harness::start(Setup {
        runner: fake,
        ..Default::default()
    })
    .await;
    h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    let mut session = h.sessions.accept().await;
    session.send(welcome(Some("stable1"), None)).await;
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    let store = Store::new(h.state_dir());
    wait_until(|| store.has_revision(&rev("stable1"))).await;
    (h, session)
}

type ServerSessionAlias = super::fake_server::ServerSession;

#[tokio::test(start_paused = true)]
async fn a_required_check_failing_past_settle_during_bake_rolls_its_module_back() {
    let fake = FakeRunner::new();
    let (h, mut session) = rolled_out(fake.clone()).await;
    fake.with_health(|revision, scope| {
        crate::testing::health(scope, !(revision.0 == "rev1" && scope == HealthScope::Bake))
    });
    let started = tokio::time::Instant::now();
    session.send(bake_request("bake-1", 600)).await;
    let rollback = session.expect(rollback_msg).await;
    let elapsed = tokio::time::Instant::now() - started;
    assert!(
        elapsed >= Duration::from_secs(120) && elapsed < Duration::from_secs(160),
        "after settle (2m), not before: {elapsed:?}"
    );
    assert_eq!(rollback.modules, vec!["m".to_string()]);
    assert_eq!(rollback.from_revision, "rev1");
    assert_eq!(rollback.to_revision, "stable1");
    assert!(rollback.succeeded);
    assert!(rollback.report_id > 0);
    assert!(rollback.reason.contains("m/api"), "{}", rollback.reason);
    assert!(rollback.evidence.iter().all(|e| !e.passed));
    // The stable revision was planned for that module only and applied.
    assert!(fake.calls().iter().any(|c| matches!(
        c,
        Call::Plan { revision, select: Some(s), .. }
            if revision.0 == "stable1" && s == "module.name == \"m\""
    )));
    assert!(
        fake.applies()
            .iter()
            .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1"))
    );
    // The bake stopped; the host stays on rev1 with the module rolled back.
    let bakes_before = bake_runs(&fake);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(bake_runs(&fake), bakes_before);
    let agent = Store::new(h.state_dir()).load_agent_state();
    assert_eq!(agent.applied_revision, Some(rev("rev1")));
    assert_eq!(agent.rolled_back_modules, vec!["m".to_string()]);
    h.stop().await.expect("clean exit");
}

fn bake_runs(fake: &FakeRunner) -> usize {
    fake.calls()
        .iter()
        .filter(|c| {
            matches!(
                c,
                Call::Health {
                    scope: HealthScope::Bake,
                    ..
                }
            )
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_check_that_recovers_inside_settle_is_a_flap_not_a_rollback() {
    let fake = FakeRunner::new();
    let (h, session) = rolled_out(fake.clone()).await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    // Fails for the first three samples (60 s at 30 s intervals), then passes.
    fake.with_health(move |_, scope| {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::testing::health(scope, scope != HealthScope::Bake || n >= 3)
    });
    session.send(bake_request("bake-1", 300)).await;
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert!(
        !fake
            .applies()
            .iter()
            .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1")),
        "no rollback"
    );
    assert!(bake_runs(&fake) >= 10, "the bake ran its whole window");
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn bake_checks_run_at_their_own_intervals_until_the_deadline() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    fake.with_health(|_, scope| HealthReport {
        scope,
        results: ["fast", "slow"]
            .into_iter()
            .map(|check| buckle_core::health::HealthResult {
                module: "m".into(),
                check: check.into(),
                kind: "http".into(),
                required: true,
                status: buckle_core::health::HealthStatus::Pass,
                at: chrono::Utc::now(),
                duration_ms: 1,
            })
            .collect(),
    });
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..Default::default()
    })
    .await;
    let module = "name: m\nhealth:\n  - name: fast\n    http: { url: 'http://127.0.0.1/a' }\n    interval: 10s\n  - name: slow\n    http: { url: 'http://127.0.0.1/b' }\n    interval: 60s\n";
    let digest = h
        .fleet
        .add_revision("rev1", bundle_with_module(FLEET_YAML, module));
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);

    let started = tokio::time::Instant::now();
    session.send(bake_request("bake-1", 125)).await;
    let mut seen: Vec<(u64, String)> = Vec::new();
    loop {
        let report = session.expect(health_report).await;
        let at = (tokio::time::Instant::now() - started).as_secs();
        for r in report.results {
            seen.push((at, r.check));
        }
        if at >= 120 {
            break;
        }
    }
    let times = |check: &str| -> Vec<u64> {
        seen.iter()
            .filter(|(_, c)| c == check)
            .map(|(t, _)| *t)
            .collect()
    };
    assert_eq!(times("slow"), vec![0, 60, 120]);
    assert_eq!(
        times("fast"),
        (0..=12).map(|i| i * 10).collect::<Vec<u64>>(),
        "every 10 s, up to the deadline"
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_final_rejection_keeps_the_agent_running_with_a_slow_backoff() {
    let mut h = Harness::start(Setup::default()).await;
    let reject = || {
        pb::server_message::Msg::Rejected(pb::Rejected {
            reason: "unknown host".into(),
            retry: false,
        })
    };
    for _ in 0..8 {
        let session = h.sessions.accept().await;
        session.send(reject()).await;
    }
    let attempts = h.fleet.attempts();
    let gaps: Vec<u64> = attempts
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs())
        .collect();
    assert_eq!(
        gaps,
        vec![1, 2, 4, 8, 16, 32, 64],
        "doubling, not a fixed restart loop"
    );
    assert!(
        h.handle.as_ref().is_some_and(|j| !j.is_finished()),
        "still running"
    );
    // Capped at an hour (128 s … 2048 s, then 3600 s rather than 4096 s).
    for _ in 0..5 {
        let session = h.sessions.accept().await;
        session.send(reject()).await;
    }
    tokio::time::sleep(Duration::from_secs(3700)).await;
    let attempts = h.fleet.attempts();
    assert!(attempts.len() >= 14);
    assert_eq!(attempts[13] - attempts[12], Duration::from_secs(3600));
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_refused_host_token_falls_back_to_the_bootstrap_token() {
    let mut h = Harness::start(Setup {
        yaml: "bootstrap_token: boot-strap\n".into(),
        prepare: Some(Box::new(|dir: &Path| {
            let store = Store::new(dir);
            store.ensure().expect("ensure");
            store.write_token("forgotten-token").expect("token");
        })),
        ..Default::default()
    })
    .await;
    let first = h.sessions.accept().await;
    assert_eq!(first.hello.token, "forgotten-token");
    assert!(!first.hello.bootstrap);
    // The admin forgot the host: its token is no longer known.
    first
        .send(pb::server_message::Msg::Rejected(pb::Rejected {
            reason: "The token is not recognised.".into(),
            retry: false,
        }))
        .await;
    let second = h.sessions.accept().await;
    assert_eq!(second.hello.token, "boot-strap");
    assert!(second.hello.bootstrap, "it registers afresh");
    second.send(welcome(None, Some("fresh-token"))).await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.read_token().ok().flatten().as_deref() == Some("fresh-token")).await;
    drop(second);
    let third = h.sessions.accept().await;
    assert_eq!(third.hello.token, "fresh-token");
    assert!(!third.hello.bootstrap);
    h.stop().await.expect("clean exit");
}

fn host_update(status: &str, labels: &[(&str, &str)]) -> pb::server_message::Msg {
    pb::server_message::Msg::HostUpdate(pb::HostUpdate {
        status: status.into(),
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    })
}

#[tokio::test(start_paused = true)]
async fn a_host_update_reaches_a_connected_agent() {
    let mut h = Harness::start(Setup::default()).await;
    let session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            status: "pending".into(),
            ..Default::default()
        }))
        .await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.load_agent_state().status.as_deref() == Some("pending")).await;
    session
        .send(host_update(
            "active",
            &[("role", "web"), ("canary", "true")],
        ))
        .await;
    wait_until(|| store.load_agent_state().status.as_deref() == Some("active")).await;
    let state = store.load_agent_state();
    assert_eq!(state.labels.get("role").map(String::as_str), Some("web"));
    assert_eq!(h.fleet.attempts().len(), 1, "no reconnect was needed");
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_host_paused_while_connected_stops_correcting_drift() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 2);
    let drift = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone(), drift.clone()]);
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        yaml: "drift: { interval: 10m, correct: true }\n".into(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    session.send(host_update("paused", &[])).await;
    let reported = session
        .expect(|m| match m {
            pb::agent_message::Msg::Drift(d) => Some(d.clone()),
            _ => None,
        })
        .await;
    assert_eq!(reported.changes, 1, "drift is still reported");
    assert!(!reported.corrected, "but a paused host does not correct it");
    assert_eq!(fake.applies().len(), 1);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn an_engine_stale_plan_is_reported_as_a_stale_plan_not_a_failure() {
    let fake = FakeRunner::new();
    let approved = plan_with_changes(&rev("rev1"), 1);
    let fresh = plan_with_changes(&rev("rev1"), 2);
    // RequestPlan and the apply's own re-plan see the approved plan; the
    // engine's check then finds the host changed; the re-plan after it sees
    // the fresh one.
    fake.with_plans(
        &rev("rev1"),
        vec![approved.clone(), approved.clone(), fresh.clone()],
    );
    let (planned, now) = (approved.digest.clone(), fresh.digest.clone());
    fake.with_apply_error(move |_| {
        Some(buckle_core::error::wrap_user(
            buckle_core::engine::StalePlan {
                planned: planned.clone(),
                fresh: now.clone(),
            },
            "The plan is stale.",
            &[],
        ))
    });
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    session.expect(plan_result).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            approved.digest.as_str(),
        )))
        .await;
    let received = session
        .collect_until(|m| plan_result(m).is_some_and(|p| p.error.starts_with("stale plan")))
        .await;
    let stale = plan_result(&received.last().expect("a report").1).expect("a plan result");
    assert_eq!(stale.digest, fresh.digest.to_string());
    assert!(
        received.iter().all(|(_, m)| apply_result(m).is_none()),
        "no failed ApplyResult"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    let state = Store::new(h.state_dir()).load_agent_state();
    assert!(
        state.unacked_reports.is_empty(),
        "nothing durable to report"
    );
    assert_eq!(state.watchdog, None);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn an_apply_re_plans_with_the_select_of_the_approved_plan() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    let select = "task.tags contains \"network\"";
    session
        .send(pb::server_message::Msg::Plan(pb::RequestPlan {
            target: String::new(),
            workload: String::new(),
            revision: "rev1".into(),
            select: select.into(),
        }))
        .await;
    session.expect(plan_result).await;
    // An older server does not echo the select: the agent remembers it.
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    // A server that echoes it is followed.
    session
        .send(pb::server_message::Msg::Apply(pb::Apply {
            select: "module.name == \"m\"".into(),
            ..apply("rev1", plan.digest.as_str())
        }))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    let selects: Vec<Option<String>> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Plan { select, .. } => Some(select),
            _ => None,
        })
        .collect();
    assert_eq!(
        selects,
        vec![
            Some(select.to_owned()),
            Some(select.to_owned()),
            Some("module.name == \"m\"".to_owned())
        ]
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn facts_refreshed_while_planning_are_reported_and_top_up_secrets() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..Default::default()
    })
    .await;
    h.fleet.set_secrets(vec![
        pb::SecretValue {
            name: "db".into(),
            value: b"hunter2".to_vec(),
            fingerprint: "01020304".into(),
        },
        pb::SecretValue {
            name: "old".into(),
            value: b"s3cret".to_vec(),
            fingerprint: "0a0b0c0d".into(),
        },
    ]);
    h.fleet.add_revision(
        "stable1",
        bundle(&format!(
            "{FLEET_YAML}secrets:\n  old: {{ file: /secrets/old }}\n"
        )),
    );
    let digest = h.fleet.add_revision(
        "rev1",
        bundle(&format!(
            "{FLEET_YAML}secrets:\n  db: {{ file: /secrets/db }}\n"
        )),
    );
    let mut session = h.sessions.accept().await;
    session.send(welcome(Some("stable1"), None)).await;
    session
        .expect(|m| matches!(m, pb::agent_message::Msg::Facts(_)).then_some(()))
        .await;
    session.send(assign("rev1", &digest)).await;

    // The engine refreshes a provider while planning: the change is reported
    // straight away, not at the next facts pass ten minutes later.
    let mut facts = buckle_core::facts::Facts::new();
    facts.mount(
        "os",
        serde_json::json!({ "family": "debian" }),
        chrono::Utc::now(),
    );
    fake.with_plan_facts(facts);
    let asked = tokio::time::Instant::now();
    session.send(request_plan("rev1")).await;
    session
        .expect(|m| match m {
            pb::agent_message::Msg::Facts(f) => {
                let tree: serde_json::Value = serde_json::from_slice(&f.json).ok()?;
                (tree["os"]["family"] == "debian").then_some(())
            }
            _ => None,
        })
        .await;
    assert!(tokio::time::Instant::now() - asked < Duration::from_secs(60));
    session.expect(plan_result).await;

    // Missing secrets were requested for the assigned and the stable revision.
    let store = Store::new(h.state_dir());
    let key = store.install_key().expect("key");
    wait_until(|| {
        store
            .load_secrets(&rev("rev1"), &key)
            .ok()
            .flatten()
            .is_some()
            && store
                .load_secrets(&rev("stable1"), &key)
                .ok()
                .flatten()
                .is_some()
    })
    .await;
    let stable = store
        .load_secrets(&rev("stable1"), &key)
        .expect("load")
        .expect("stable topped up");
    assert!(stable.get("old").is_some());
    // An unchanged re-plan reports nothing new.
    session.send(request_plan("rev1")).await;
    let between = session.collect_until(|m| plan_result(m).is_some()).await;
    assert!(
        between
            .iter()
            .all(|(_, m)| !matches!(m, pb::agent_message::Msg::Facts(_))),
        "unchanged facts are not re-sent"
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_stable_revision_that_does_not_match_its_named_digest_is_not_cached() {
    let mut h = Harness::start(Setup::default()).await;
    let digest = h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    let wrong = format!("sha256:{}", crate::bundle::sha256_hex(b"something else"));
    let mut session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            stable_revision: "stable1".into(),
            stable_tree_digest: wrong,
            status: "active".into(),
            ..Default::default()
        }))
        .await;
    // A background cache, not an assignment: no plan failure is reported.
    let until_beat = session
        .collect_until(|m| matches!(m, pb::agent_message::Msg::Heartbeat(_)))
        .await;
    assert!(until_beat.iter().all(|(_, m)| plan_result(m).is_none()));
    assert!(!h.state_dir().join("revisions/stable1").exists());
    session.close();

    let session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            stable_revision: "stable1".into(),
            stable_tree_digest: digest,
            status: "active".into(),
            ..Default::default()
        }))
        .await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.has_revision(&rev("stable1"))).await;
    assert!(store.load_agent_state().stable_tree_digest.is_some());
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_session_without_a_welcome_times_out_and_reconnects() {
    let mut h = Harness::start(Setup::default()).await;
    let _silent = h.sessions.accept().await;
    let opened = tokio::time::Instant::now();
    let _next = h.sessions.accept().await;
    let waited = tokio::time::Instant::now() - opened;
    assert!(
        waited >= Duration::from_secs(30) && waited < Duration::from_secs(40),
        "{waited:?}"
    );
    h.stop().await.expect("clean exit");
}

fn secret(name: &str, value: &[u8], fingerprint: &str) -> pb::SecretValue {
    pb::SecretValue {
        name: name.into(),
        value: value.to_vec(),
        fingerprint: fingerprint.into(),
    }
}

fn assign_with(
    revision: &str,
    digest: &str,
    secrets: Vec<pb::SecretValue>,
) -> pb::server_message::Msg {
    pb::server_message::Msg::Assign(pb::Assign {
        revision: revision.into(),
        tree_digest: digest.into(),
        secrets: Some(pb::SecretBundle { secrets }),
        ..Default::default()
    })
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_the_running_apply_and_starts_nothing_new() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let (mut h, runner) = gated(fake.clone()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    let later = h.fleet.add_revision("rev2", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;
    // Queued behind the running apply.
    session.send(assign("rev2", &later)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    h.shutdown.cancel();
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert!(
        h.handle.as_ref().is_some_and(|j| !j.is_finished()),
        "still waiting for the apply after five minutes"
    );
    runner.release.notify_one();
    assert_eq!(h.join().await.expect("clean exit"), 0);
    let store = Store::new(h.state_dir());
    assert_eq!(store.load_agent_state().applied_revision, Some(rev("rev1")));
    assert!(
        !store.has_revision(&rev("rev2")),
        "the queued assignment never started"
    );
}

#[tokio::test(start_paused = true)]
async fn the_watchdog_fallback_survives_a_new_stable_revision() {
    let fake = FakeRunner::new();
    let (h, session) = rolled_out(fake.clone()).await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.load_agent_state().watchdog.is_some()).await;
    // A newer stable revision arrives while the watchdog still points at
    // stable1: stable1's tree must stay for the fallback.
    let digest = h.fleet.add_revision("stable2", bundle(FLEET_YAML));
    session
        .send(pb::server_message::Msg::Assign(pb::Assign {
            revision: "stable2".into(),
            stable: true,
            tree_digest: digest,
            ..Default::default()
        }))
        .await;
    wait_until(|| store.has_revision(&rev("stable2"))).await;
    assert!(store.has_revision(&rev("stable1")));
    assert_eq!(
        store.load_agent_state().watchdog.map(|w| w.to),
        Some(rev("stable1"))
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn secrets_of_a_revision_that_never_arrived_are_pruned_once_unwanted() {
    let mut h = Harness::start(Setup::default()).await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    // rev-gone is not on the server: its tree never arrives, its secrets do.
    session
        .send(assign_with(
            "rev-gone",
            "",
            vec![secret("db", b"hunter2", "01020304")],
        ))
        .await;
    session.expect(plan_result).await;
    let secrets = h.state_dir().join("secrets/rev-gone.cbor");
    assert!(secrets.is_file());
    let digest = h.fleet.add_revision("rev2", bundle(FLEET_YAML));
    session.send(assign("rev2", &digest)).await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.has_revision(&rev("rev2"))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!secrets.exists(), "pruned with the revision it belonged to");
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn health_reasons_never_carry_secret_values() {
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
        ..Default::default()
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
    session
        .send(pb::server_message::Msg::Health(pb::RunHealth {
            request_id: "h1".into(),
            scope: pb::HealthScope::Manual as i32,
            ..Default::default()
        }))
        .await;
    let report = session.expect(health_report).await;
    let reason = &report.results[0].reason;
    assert!(!reason.contains("hunter2"), "{reason}");
    assert!(
        reason.contains("401"),
        "the rest of the reason stays: {reason}"
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_local_rollback_renders_the_stable_revision_with_both_revisions_secrets() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    fake.with_plans(&rev("stable1"), vec![plan_with_changes(&rev("stable1"), 1)]);
    fake.with_health(|revision, scope| crate::testing::health(scope, revision.0 != "rev1"));
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        ..Default::default()
    })
    .await;
    h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    let mut session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            stable_revision: "stable1".into(),
            status: "active".into(),
            secrets: Some(pb::SecretBundle {
                secrets: vec![secret("old", b"o", "0a0a0a0a")],
            }),
            ..Default::default()
        }))
        .await;
    session
        .send(assign_with(
            "rev1",
            &digest,
            vec![secret("new", b"n", "0b0b0b0b")],
        ))
        .await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    let store = Store::new(h.state_dir());
    wait_until(|| store.has_revision(&rev("stable1"))).await;
    h.fleet.set_unreachable(true);
    session.close();
    tokio::time::timeout(
        Duration::from_secs(3600),
        fake.wait_for(|calls| {
            calls
                .iter()
                .any(|c| matches!(c, Call::Apply { revision, .. } if revision.0 == "stable1"))
        }),
    )
    .await
    .expect("rolled back");
    let seen = fake.applied_secrets();
    let stable = seen
        .iter()
        .find(|(r, _)| r.0 == "stable1")
        .expect("the rollback apply");
    assert_eq!(stable.1, vec!["new".to_string(), "old".to_string()]);
    h.stop().await.expect("clean exit");
}

#[rstest::rstest]
#[case::when_idle(true)]
#[case::right_away(false)]
#[tokio::test(start_paused = true)]
async fn an_upgrade_when_idle_waits_for_queued_operations(#[case] when_idle: bool) {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone()]);
    let (mut h, runner) = gated(fake.clone()).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    let later = h.fleet.add_revision("rev2", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    runner.started.notified().await;
    session
        .send(pb::server_message::Msg::Upgrade(pb::Upgrade {
            version: "2.1.0".into(),
            when_idle,
        }))
        .await;
    session.send(assign("rev2", &later)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    runner.release.notify_one();
    assert_eq!(h.join().await.expect("exits after upgrading"), 0);
    assert_eq!(
        *h.updater.requested.lock().expect("lock"),
        vec![Some("2.1.0".to_string())]
    );
    assert_eq!(
        Store::new(h.state_dir()).has_revision(&rev("rev2")),
        when_idle,
        "the queued assignment runs first only when the upgrade waits for idle"
    );
}

#[tokio::test(start_paused = true)]
async fn a_check_failing_at_the_end_of_the_bake_is_followed_through_settle() {
    let fake = FakeRunner::new();
    let (h, mut session) = rolled_out(fake.clone()).await;
    let started = tokio::time::Instant::now();
    // Healthy at the first sample of a 60 s bake, failing from the second
    // (30 s) on: settle (2 m) runs out well after the window closes.
    fake.with_health(move |revision, scope| {
        let late = tokio::time::Instant::now() - started >= Duration::from_secs(20);
        crate::testing::health(
            scope,
            !(revision.0 == "rev1" && scope == HealthScope::Bake && late),
        )
    });
    session.send(bake_request("bake-1", 60)).await;
    let rollback = session.expect(rollback_msg).await;
    let elapsed = tokio::time::Instant::now() - started;
    assert!(
        elapsed >= Duration::from_secs(150) && elapsed < Duration::from_secs(200),
        "settle (2m) after the first failing sample: {elapsed:?}"
    );
    assert_eq!(rollback.modules, vec!["m".to_string()]);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn drift_passes_plan_and_correct_in_drift_mode() {
    let fake = FakeRunner::new();
    let plan = plan_with_changes(&rev("rev1"), 2);
    let drift = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![plan.clone(), drift]);
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        yaml: "drift: { interval: 10m, correct: true }\n".into(),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    let digest = h.fleet.add_revision("rev1", bundle(FLEET_YAML));
    session.send(assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(apply(
            "rev1",
            plan.digest.as_str(),
        )))
        .await;
    assert!(session.expect(apply_result).await.succeeded);
    let reported = session
        .expect(|m| match m {
            pb::agent_message::Msg::Drift(d) => Some(d.clone()),
            _ => None,
        })
        .await;
    assert!(reported.corrected);
    let flags = fake.drift_flags();
    // The approved apply: a normal plan and apply.
    assert_eq!(flags[0], ("plan", rev("rev1"), false));
    assert_eq!(flags[1], ("apply", rev("rev1"), false));
    // The drift pass: both its plan and its correction in drift mode (which
    // the engine runner turns into no removals).
    assert_eq!(
        &flags[2..4],
        &[("plan", rev("rev1"), true), ("apply", rev("rev1"), true)]
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_pending_host_caches_the_stable_revision_only_once_it_is_active() {
    let mut h = Harness::start(Setup::default()).await;
    h.fleet.add_revision("stable1", bundle(FLEET_YAML));
    let mut session = h.sessions.accept().await;
    session
        .send(pb::server_message::Msg::Welcome(pb::Welcome {
            stable_revision: "stable1".into(),
            status: "pending".into(),
            ..Default::default()
        }))
        .await;
    let until_beat = session
        .collect_until(|m| matches!(m, pb::agent_message::Msg::Heartbeat(_)))
        .await;
    assert!(until_beat.iter().all(|(_, m)| plan_result(m).is_none()));
    let store = Store::new(h.state_dir());
    assert!(
        !store.has_revision(&rev("stable1")),
        "a pending host fetches nothing"
    );
    session
        .send(pb::server_message::Msg::HostUpdate(pb::HostUpdate {
            status: "active".into(),
            stable_revision: "stable1".into(),
            ..Default::default()
        }))
        .await;
    wait_until(|| store.has_revision(&rev("stable1"))).await;
    h.stop().await.expect("clean exit");
}

const CLUSTER_FLEET: &str = "version: 2\nfleet:\n  clusters:\n    home-nomad: { kind: nomad }\n";

fn targeted_assign(revision: &str, digest: &str) -> pb::server_message::Msg {
    pb::server_message::Msg::Assign(pb::Assign {
        revision: revision.into(),
        tree_digest: digest.into(),
        target: "home-nomad".into(),
        cluster: Some(pb::ClusterTarget {
            kind: "nomad".into(),
            identity: "buckle_jwt".into(),
            fingerprint_key: vec![7; 32],
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[tokio::test(start_paused = true)]
async fn an_executor_plans_applies_and_checks_a_cluster_target_beside_its_host() {
    let fake = FakeRunner::new();
    let host_plan = plan_with_changes(&rev("rev1"), 1);
    fake.with_plans(&rev("rev1"), vec![host_plan.clone()]);
    let nomad = buckle_nomad::testing::FakeNomad::new();
    let mut h = Harness::start(Setup {
        runner: fake.clone(),
        cluster: Some(nomad.clone()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    assert!(session.hello.capabilities.contains(&"targets".to_owned()));
    session.send(welcome(None, None)).await;
    let job = r#"{"ID": "paste", "Type": "service", "TaskGroups": [{"Name": "api", "Count": 1, "Tasks": [{"Name": "paste"}]}]}"#;
    let digest = h.fleet.add_revision(
        "rev1",
        crate::tests::bundle_files(&[
            ("buckle.yaml", CLUSTER_FLEET),
            (
                "workloads/paste/workload.yaml",
                "name: paste\nresources: [paste.nomad.hcl]\n",
            ),
            ("workloads/paste/files/paste.nomad.hcl", job),
        ]),
    );

    // Plan: answered for the target, logged in with a deploy identity.
    session.send(targeted_assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Plan(pb::RequestPlan {
            target: "home-nomad".into(),
            workload: String::new(),
            revision: "rev1".into(),
            select: String::new(),
        }))
        .await;
    let planned = session.expect(plan_result).await;
    assert_eq!(
        (planned.target.as_str(), planned.error.as_str()),
        ("home-nomad", "")
    );
    assert!(planned.changes > 0, "{planned:?}");
    assert!(nomad.logins()[0].contains("Deploy"), "{:?}", nomad.logins());

    // Apply: every event and the durable result name the target.
    session.send(targeted_assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Apply(pb::Apply {
            target: "home-nomad".into(),
            approval_digest: planned.approval_digest.clone(),
            ..apply("rev1", &planned.digest)
        }))
        .await;
    let seen = session.collect_until(|m| apply_result(m).is_some()).await;
    let events: Vec<pb::ApplyEvent> = seen
        .iter()
        .filter_map(|(_, m)| match m {
            pb::agent_message::Msg::Apply(e) => Some(e.clone()),
            _ => None,
        })
        .collect();
    assert!(!events.is_empty());
    assert!(
        events.iter().all(|e| e.target == "home-nomad"),
        "{events:?}"
    );
    let result = seen.iter().find_map(|(_, m)| apply_result(m)).unwrap();
    assert_eq!(result.target, "home-nomad");
    assert!(result.succeeded, "{result:?}");
    assert_eq!(result.applied_revision, "rev1");
    assert!(nomad.job("default", "paste").is_some());

    // Health of the cluster's applied revision.
    session.send(targeted_assign("rev1", &digest)).await;
    session
        .send(pb::server_message::Msg::Health(pb::RunHealth {
            request_id: "h1".into(),
            scope: pb::HealthScope::Bake as i32,
            target: "home-nomad".into(),
            revision: "rev1".into(),
            ..Default::default()
        }))
        .await;
    let report = session.expect(health_report).await;
    assert_eq!(report.target, "home-nomad");
    assert!(
        !report.results.is_empty() && report.results.iter().all(|r| r.passed),
        "{report:?}"
    );

    // The host's own work is unaffected and names no target.
    session.send(assign("rev1", &digest)).await;
    session.send(request_plan("rev1")).await;
    let own = session.expect(plan_result).await;
    assert_eq!(
        (own.target.as_str(), own.digest.as_str()),
        ("", host_plan.digest.as_str())
    );
    assert!(
        !fake.calls().iter().any(|c| matches!(c, Call::Apply { .. })),
        "the cluster's apply must not run on the host"
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn targeted_work_without_its_assignment_fails_with_a_reason() {
    let mut h = Harness::start(Setup {
        cluster: Some(buckle_nomad::testing::FakeNomad::new()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session
        .send(pb::server_message::Msg::Plan(pb::RequestPlan {
            target: "home-nomad".into(),
            workload: String::new(),
            revision: "rev1".into(),
            select: String::new(),
        }))
        .await;
    let refused = session.expect(plan_result).await;
    assert_eq!(refused.target, "home-nomad");
    assert!(refused.error.contains("no assignment"), "{refused:?}");
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn an_in_cluster_executor_presents_its_current_workload_identity() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("nomad_buckle.jwt");
    std::fs::write(&token_file, "jwt-one\n").unwrap();
    let mut h = Harness::start(Setup {
        yaml: format!(
            "workload_identity: {{ token_file: {} }}\n",
            token_file.display()
        ),
        ..Default::default()
    })
    .await;
    let first = h.sessions.accept().await;
    assert_eq!(first.hello.workload_token, "jwt-one");
    assert_eq!(first.hello.host_id, "test-host");
    first.send(welcome(None, Some("session-token"))).await;
    let store = Store::new(h.state_dir());
    wait_until(|| store.read_token().ok().flatten().as_deref() == Some("session-token")).await;

    // The scheduler rotated the token; the next session presents the new one.
    std::fs::write(&token_file, "jwt-two").unwrap();
    drop(first);
    let second = h.sessions.accept().await;
    assert_eq!(second.hello.workload_token, "jwt-two");
    h.stop().await.expect("clean exit");
}
