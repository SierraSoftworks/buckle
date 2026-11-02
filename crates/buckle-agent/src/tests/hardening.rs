//! Scenarios for the design-conformance hardening: single writer of the
//! engine state, durable reports, session-bound applies, bake rollback,
//! rejection handling and the rest.

use super::*;
use crate::runner::{RunContext, Runner};
use crate::testing::plan_with_changes;
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
