//! Scenario 10: the controller is a persisted state machine (design §10
//! "Decision: The controller is a persisted state machine"): every
//! transition is a row, and a server restart over the same SQLite data
//! directory resumes the rollout where it was, rebuilding timers and
//! connections from the database and the reconnecting agents.

use crate::common::{self, APP_CONF};
use buckle_api::{Connection, HostRolloutState, PhaseState, RolloutDetail, RolloutState};
use buckle_integration::{World, describe, phase, rollout_host};

const PHASES: &str = "    - { name: canary, select: 'host.groups contains \"canary\"', parallel: 1, approval: required, bake: 1s }\n    - { name: rest, select: 'true', parallel: 2, approval: auto, bake: 3s }\n";
const HOSTS: [&str; 3] = ["canary-1", "web-1", "web-2"];

/// A converged three-host fleet with a rollout of revision 2 in progress.
async fn fleet_with_rollout() -> (World, String, String) {
    let mut world = World::start(common::app(PHASES, 1)).await;
    let v1 = world.source_revision();
    world.spawn(common::host("canary-1").label("canary", "true"));
    world.spawn(common::host("web-1"));
    world.spawn(common::host("web-2"));
    world.converge(&v1, &HOSTS).await;
    let v2 = world.publish(&common::app(PHASES, 2)).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    (world, v2, id)
}

async fn reconnected(world: &World) {
    for h in HOSTS {
        world
            .wait_host(h, "reconnected", |s| s.connection == Connection::Online)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollout_restarted_mid_bake_resumes_where_it_was_and_completes() {
    let (mut world, v2, id) = fleet_with_rollout().await;
    world
        .wait_rollout(&id, "awaiting approval", |d| {
            d.summary.state == RolloutState::AwaitingApproval
        })
        .await;
    world.approve(&id, "canary").await;
    world
        .wait_rollout(&id, "the rest phase baking", |d| {
            ["web-1", "web-2"]
                .iter()
                .all(|h| rollout_host(d, h).is_some_and(|r| r.state == HostRolloutState::Baking))
        })
        .await;

    world.restart_server().await;
    let resumed: RolloutDetail = world.api().get(&format!("/api/v1/rollouts/{id}")).await;
    assert_eq!(
        rollout_host(&resumed, "canary-1").map(|r| r.state),
        Some(HostRolloutState::Healthy),
        "finished hosts stay finished: {}",
        describe(&resumed)
    );
    assert_eq!(
        phase(&resumed, "canary").map(|p| p.state),
        Some(PhaseState::Done)
    );
    assert_eq!(
        phase(&resumed, "canary").and_then(|p| p.approved_by.clone()),
        Some("anonymous".to_owned()),
        "the approval survives"
    );
    for h in ["web-1", "web-2"] {
        assert!(
            matches!(
                rollout_host(&resumed, h).map(|r| r.state),
                Some(HostRolloutState::Baking | HostRolloutState::Healthy)
            ),
            "{h} resumed its bake: {}",
            describe(&resumed)
        );
    }
    reconnected(&world).await;

    let done = world
        .wait_rollout(&id, "completed", |d| {
            d.summary.state == RolloutState::Completed
        })
        .await;
    for h in HOSTS {
        let row = rollout_host(&done, h).unwrap();
        assert_eq!(row.state, HostRolloutState::Healthy, "{h}");
        assert_ne!(row.phase, "stragglers", "{h} finished in its own phase");
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(2)),
            "{h}"
        );
    }
    world
        .wait_fleet("v2 to be stable", |f| {
            f.stable_revision.as_deref() == Some(v2.as_str())
        })
        .await;
    let rollouts: buckle_api::Page<buckle_api::RolloutSummary> =
        world.api().get("/api/v1/rollouts").await;
    assert_eq!(
        rollouts.items.iter().filter(|r| r.revision == v2).count(),
        1,
        "one rollout of v2 across two server instances"
    );
    let kinds: Vec<&str> = done.events.iter().map(|e| e.kind.as_str()).collect();
    for kind in ["rollout.approve", "rollout.phase.done", "revision.promoted"] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    world.shutdown().await;
}

/// Design §10: a restart resumes mid-rollout; the in-memory part (timers and
/// connected streams) is rebuilt from the database and the reconnecting
/// agents. Hosts still to plan or apply are not taken for offline (missed)
/// until they reconnect or the disconnect timeout passes, so a phase awaiting
/// approval keeps awaiting it and nothing is promoted unapproved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollout_restarted_while_awaiting_approval_still_awaits_it() {
    let (mut world, _v2, id) = fleet_with_rollout().await;
    let before = world
        .wait_rollout(&id, "awaiting approval with every plan", |d| {
            d.summary.state == RolloutState::AwaitingApproval
                && HOSTS
                    .iter()
                    .all(|h| rollout_host(d, h).is_some_and(|r| r.plan_digest.is_some()))
        })
        .await;

    world.restart_server().await;
    reconnected(&world).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let after: RolloutDetail = world.api().get(&format!("/api/v1/rollouts/{id}")).await;
    for h in HOSTS {
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(1)),
            "{h} applied v2 without the canary's approval: {}",
            describe(&after)
        );
    }
    assert_eq!(
        after.summary.state,
        RolloutState::AwaitingApproval,
        "{}",
        describe(&after)
    );
    for h in HOSTS {
        assert_eq!(
            rollout_host(&after, h).map(|r| (&r.phase, r.state, &r.plan_digest)),
            rollout_host(&before, h).map(|r| (&r.phase, r.state, &r.plan_digest)),
            "{h}"
        );
    }
    world.shutdown().await;
}
