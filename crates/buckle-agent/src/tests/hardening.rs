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
    assert_eq!(attempts.len(), 14);
    let last = attempts[attempts.len() - 1] - attempts[attempts.len() - 2];
    assert_eq!(last, Duration::from_secs(3600));
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
