//! Scenario 7: an agent rolls itself back when a change cuts it off from the
//! server and health fails (design §10 "Agents can roll themselves back"),
//! then reports the LocalRollback to a fresh server instance over the same
//! data directory, which pauses the rollout.

use crate::common::{self, APP_CONF, HEALTHY};
use buckle_api::{HostRolloutState, Page, RolloutState};
use buckle_core::system::Command;
use buckle_core::testing::ok_output;
use buckle_integration::server::ServerSettings;
use buckle_integration::{AgentSpec, Fleet, WAIT, World, eventually, rollout_host};

const PHASE: &str =
    "    - { name: fleet, select: 'true', parallel: 1, approval: auto, bake: 60s }\n";
const POLICY: &str = "  local_rollback: { enabled: true, after: 2s, on_health: true }\n";

/// Revision 2: the checked `app` with its new file, which applies and passes
/// its checks, then a `network` change after it that breaks the service (its
/// health marker goes) and cuts the host off (the scripted `restart-network`
/// cuts the proxy).
///
/// Both breakages happen during the apply, before the watchdog arms, so
/// nothing the test does afterwards races the `local_rollback.after` window:
/// it closes with no session and checks failing past settle, every time.
fn breaking_v2() -> Fleet {
    common::checked_app_module(Fleet::with_phases(POLICY, PHASE), 2).module(
        "network",
        format!(
            "name: network\nneeds: [app]\ntasks:\n  - file.absent: {{ path: {HEALTHY} }}\n  - command.run: {{ run: [restart-network] }}\n"
        ),
    )
}

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
    let cutter = world.cutter();
    let edge = world.spawn(AgentSpec::new("edge-1").system(
        common::healthy_system("edge-1").on_command("restart-network", &[], move |_: &Command| {
            cutter.cut();
            ok_output("")
        }),
    ));
    world.converge(&v1, &["edge-1"]).await;

    // The agent is connected when the apply starts, so it arms its watchdog
    // after it; the apply itself cuts it off and breaks the service.
    let v2 = world.publish(&breaking_v2()).await.id;

    // Past local_rollback.after with checks failing past settle and no
    // session, the agent applies the cached stable revision on its own.
    let record = eventually("the agent to roll itself back", WAIT, async || {
        local_rollbacks(&edge.agent_state())
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
    let id = world.rollout_for(&v2).await.summary.id;
    world.stop_server().await;

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
        local_rollbacks(&edge.agent_state())
            .is_empty()
            .then_some(())
            .ok_or("still pending".to_owned())
    })
    .await;
    world.shutdown().await;
}

/// Local rollbacks the agent holds until the server acknowledges them.
fn local_rollbacks(
    state: &buckle_agent::store::AgentState,
) -> Vec<buckle_agent::transport::LocalRollbackRecord> {
    state
        .unacked_reports
        .iter()
        .filter_map(|r| match r {
            buckle_agent::transport::DurableReport::LocalRollback(record) => Some(record.clone()),
            _ => None,
        })
        .collect()
}
