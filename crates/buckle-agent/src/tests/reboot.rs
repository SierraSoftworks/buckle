//! Reboots a run asks for: after the result is stored, once, and never from
//! an unattended drift correction.

use super::fake_server::ServerSession;
use super::*;
use crate::store::{AgentState, PendingReboot};
use crate::testing::{plan_with_changes, succeeded};
use buckle_core::plan::{Plan, RevisionId};
use buckle_core::reboot::RebootRequest;
use std::time::Duration;
use tokio::time::Instant;

fn rev(id: &str) -> RevisionId {
    RevisionId::new(id)
}

fn request(delay: u64) -> RebootRequest {
    RebootRequest {
        requested_by: vec!["pi:reboot".into()],
        ..RebootRequest::new(Duration::from_secs(delay), Duration::from_secs(3600))
    }
}

/// A runner whose applies of `plan` ask for a reboot.
fn rebooting_runner(plan: &Plan) -> Arc<FakeRunner> {
    let runner = FakeRunner::new();
    runner.with_plans(&plan.revision, vec![plan.clone()]);
    let digest = plan.digest.clone();
    runner.with_outcome(move |p| {
        let mut outcome = succeeded(p);
        if p.digest == digest {
            outcome.reboot = Some(request(5));
        }
        outcome
    });
    runner
}

async fn start(runner: Arc<FakeRunner>, yaml: &str) -> (Harness, ServerSession) {
    let mut h = Harness::start(Setup {
        runner,
        yaml: yaml.into(),
        ..Default::default()
    })
    .await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    (h, session)
}

async fn roll_out(h: &Harness, session: &mut ServerSession, id: &str) -> pb::ApplyResult {
    let digest = h.fleet.add_revision(id, bundle(FLEET_YAML));
    session.send(assign(id, &digest)).await;
    session.send(request_plan(id)).await;
    let planned = session.expect(plan_result).await;
    session
        .send(pb::server_message::Msg::Apply(apply(id, &planned.digest)))
        .await;
    session.expect(apply_result).await
}

async fn ack(session: &ServerSession, result: &pb::ApplyResult) {
    session
        .send(pb::server_message::Msg::Ack(pb::Ack {
            report_ids: vec![result.report_id],
        }))
        .await;
}

async fn wait_for_reboot(runner: &FakeRunner) -> Instant {
    tokio::time::timeout(
        Duration::from_secs(3600),
        runner.wait_for(|calls| calls.contains(&crate::testing::Call::Reboot)),
    )
    .await
    .expect("rebooted");
    Instant::now()
}

fn pending(h: &Harness) -> Option<PendingReboot> {
    Store::new(h.state_dir()).load_agent_state().pending_reboot
}

#[tokio::test(start_paused = true)]
async fn the_result_announces_the_reboot_which_follows_its_acknowledgement() {
    let plan = plan_with_changes(&rev("rev1"), 1);
    let runner = rebooting_runner(&plan);
    let (h, mut session) = start(runner.clone(), "").await;
    let result = roll_out(&h, &mut session, "rev1").await;
    let reboot = result
        .reboot
        .clone()
        .expect("the result announces the reboot");
    assert_eq!(reboot.boot_id, "boot-1");
    assert_eq!(reboot.requested_by, vec!["pi:reboot"]);
    assert_eq!(reboot.timeout.map(|d| d.seconds), Some(3600));

    // Not before the server has stored the result.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(runner.reboots(), 0);
    assert!(pending(&h).is_some_and(|p| p.issued_at.is_none()));

    let acked = Instant::now();
    ack(&session, &result).await;
    let rebooted = wait_for_reboot(&runner).await;
    assert!(
        rebooted - acked >= Duration::from_secs(5),
        "after the delay"
    );
    assert_eq!(runner.reboots(), 1);
    assert!(pending(&h).is_some_and(|p| p.issued_at.is_some()));
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn without_an_acknowledgement_the_agent_reboots_after_the_grace() {
    let plan = plan_with_changes(&rev("rev1"), 1);
    let runner = rebooting_runner(&plan);
    let (h, mut session) = start(runner.clone(), "").await;
    let reported = Instant::now();
    roll_out(&h, &mut session, "rev1").await;
    let rebooted = wait_for_reboot(&runner).await;
    assert!(
        rebooted - reported >= Duration::from_secs(65),
        "grace and delay"
    );
    // The result is still kept, to be sent again after the reboot.
    assert!(
        !Store::new(h.state_dir())
            .load_agent_state()
            .unacked_reports
            .is_empty()
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_reboot_that_cannot_start_is_reported_and_forgotten() {
    let plan = plan_with_changes(&rev("rev1"), 1);
    let runner = rebooting_runner(&plan);
    runner.with_reboot_error("Access denied");
    let (h, mut session) = start(runner.clone(), "").await;
    let result = roll_out(&h, &mut session, "rev1").await;
    ack(&session, &result).await;
    let failed = session
        .expect(|m| match m {
            pb::agent_message::Msg::RebootFailed(f) => Some(f.clone()),
            _ => None,
        })
        .await;
    assert_eq!(failed.boot_id, "boot-1");
    assert!(failed.error.contains("Access denied"), "{}", failed.error);
    assert_eq!(pending(&h), None);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_drift_correction_never_reboots() {
    let plan = plan_with_changes(&rev("rev1"), 2);
    let drift = plan_with_changes(&rev("rev1"), 1);
    let runner = FakeRunner::new();
    runner.with_plans(
        &rev("rev1"),
        vec![plan.clone(), plan.clone(), drift.clone()],
    );
    let digest = drift.digest.clone();
    runner.with_outcome(move |p| {
        let mut outcome = succeeded(p);
        if p.digest == digest {
            outcome.reboot = Some(request(0));
        }
        outcome
    });
    let (h, mut session) = start(runner.clone(), "drift: { interval: 10m, correct: true }\n").await;
    let result = roll_out(&h, &mut session, "rev1").await;
    assert!(result.reboot.is_none());
    let corrected = session
        .expect(|m| match m {
            pb::agent_message::Msg::Drift(d) => Some(d.corrected),
            _ => None,
        })
        .await;
    assert!(corrected);
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(runner.reboots(), 0);
    assert_eq!(pending(&h), None);
    h.stop().await.expect("clean exit");
}

/// Starts an agent whose agent.json holds a pending reboot from `boot`.
async fn start_with_pending(boot: &str, issued: bool) -> Harness {
    let boot = boot.to_owned();
    Harness::start(Setup {
        prepare: Some(Box::new(move |state_dir: &Path| {
            let store = Store::new(state_dir);
            store.ensure().expect("ensure");
            let mut request = request(1);
            request.boot_id = Some(boot);
            store
                .save_agent_state(&AgentState {
                    pending_reboot: Some(PendingReboot {
                        report_id: 7,
                        request,
                        requested_at: chrono::Utc::now(),
                        issued_at: issued.then(chrono::Utc::now),
                    }),
                    ..Default::default()
                })
                .expect("agent state");
        })),
        ..Default::default()
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn back_from_the_reboot_the_agent_forgets_it_and_names_the_new_boot() {
    let mut h = start_with_pending("boot-0", true).await;
    let session = h.sessions.accept().await;
    assert_eq!(session.hello.boot_id, "boot-1");
    session.send(welcome(None, None)).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(h.runner.reboots(), 0);
    assert_eq!(pending(&h), None);
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_restart_before_the_reboot_carries_it_out() {
    let mut h = start_with_pending("boot-1", false).await;
    let session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    wait_for_reboot(&h.runner).await;
    h.stop().await.expect("clean exit");
}
