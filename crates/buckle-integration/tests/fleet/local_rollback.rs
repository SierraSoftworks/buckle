//! Scenario 7: an agent rolls itself back when a change cuts it off from the
//! server and health fails (design §10 "Agents can roll themselves back"),
//! then reports the LocalRollback to a fresh server instance over the same
//! data directory, which pauses the rollout.

use crate::common::{self, APP_CONF, HEALTHY};
use buckle_api::{HostRolloutState, Page, RolloutState};
use buckle_integration::server::ServerSettings;
use buckle_integration::{Fleet, WAIT, World, eventually, rollout_host};

const PHASE: &str =
    "    - { name: fleet, select: 'true', parallel: 1, approval: auto, bake: 60s }\n";
const POLICY: &str = "  local_rollback: { enabled: true, after: 2s, on_health: true }\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_off_agent_with_failing_health_reapplies_the_stable_revision_and_reports_it_on_reconnect()
 {
    let mut world = World::start_with(
        common::app_module(Fleet::with_phases(POLICY, PHASE), 1),
        ServerSettings {
            disconnect_timeout: std::time::Duration::from_secs(120),
            ..Default::default()
        },
    )
    .await;
    let v1 = world.source_revision();
    let edge = world.spawn(common::host("edge-1"));
    world.converge(&v1, &["edge-1"]).await;

    // The new revision applies and passes its checks; the agent was
    // connected before the apply, so it arms its watchdog.
    let v2 = world
        .publish(&common::checked_app_module(
            Fleet::with_phases(POLICY, PHASE),
            2,
        ))
        .await
        .id;
    let id = world.rollout_for(&v2).await.summary.id;
    world
        .wait_rollout_host(&id, "edge-1", HostRolloutState::Baking)
        .await;
    eventually("the watchdog to arm", WAIT, async || {
        edge.agent_state()
            .watchdog
            .filter(|w| w.from.0 == v2 && w.to.0 == v1)
            .map(|_| ())
            .ok_or("not armed".to_owned())
    })
    .await;
    assert_eq!(edge.file(APP_CONF), Some(common::app_conf(2)));

    // The server goes away and the service the change touched breaks.
    world.stop_server().await;
    edge.remove_file(HEALTHY).await;

    // Past local_rollback.after with checks failing past settle and no
    // session, the agent applies the cached stable revision on its own.
    let record = eventually("the agent to roll itself back", WAIT, async || {
        edge.agent_state()
            .pending_rollbacks
            .first()
            .cloned()
            .ok_or("no local rollback yet".to_owned())
    })
    .await;
    assert_eq!(record.from.0, v2);
    assert_eq!(record.to.0, v1);
    assert!(record.succeeded, "{}", record.reason);
    assert!(
        record
            .evidence
            .iter()
            .any(|r| r.module == "app" && r.check == "healthy" && !r.status.is_pass()),
        "{:?}",
        record.evidence
    );
    assert_eq!(edge.file(APP_CONF), Some(common::app_conf(1)));
    assert_eq!(edge.applied_revision().as_deref(), Some(v1.as_str()));

    // A fresh server over the same data directory resumes the rollout; the
    // agent reconnects and reports the rollback, which fails its wave.
    world.start_server().await;
    let detail = world
        .wait_rollout(&id, "paused by the local rollback", |d| {
            d.summary.state == RolloutState::Paused
                && rollout_host(d, "edge-1").is_some_and(|h| h.state == HostRolloutState::Failed)
        })
        .await;
    let reason = detail.summary.pause_reason.clone().unwrap_or_default();
    assert!(reason.contains("rolled itself back"), "{reason}");
    let row = rollout_host(&detail, "edge-1").unwrap();
    assert_eq!(row.failed_checks, vec!["app/healthy".to_owned()]);
    world
        .wait_host("edge-1", "back on the stable revision", |h| {
            h.applied_revision.as_deref() == Some(v1.as_str())
        })
        .await;
    let audit: Page<serde_json::Value> = world.api().get("/api/v1/audit?limit=1000").await;
    assert!(
        audit
            .items
            .iter()
            .any(|e| e["action"] == "host.local_rollback" && e["subject"] == "edge-1"),
        "{:?}",
        audit.items
    );
    eventually("the report to leave the agent's queue", WAIT, async || {
        edge.agent_state()
            .pending_rollbacks
            .is_empty()
            .then_some(())
            .ok_or("still pending".to_owned())
    })
    .await;
    world.shutdown().await;
}
