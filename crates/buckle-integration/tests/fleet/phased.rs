//! Scenario 3: a phased rollout. The canary phase waits for sign-off and
//! approval applies only the canary; after its bake and the pre-phase gate the
//! rollout advances. A host enrolled mid-rollout joins its phase when that
//! phase has not started yet; one whose phase has already run waits for the
//! stragglers stage, which promotion opens (design §10 "Controller
//! algorithm").

use crate::common::{self, APP_CONF, CANARY_THEN_REST};
use buckle_api::{HostRolloutState, PhaseState, RolloutState};
use buckle_integration::{World, phase, rollout_host};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canary_approval_applies_only_the_canary_then_the_rollout_advances_and_stragglers_converge()
{
    let mut world = World::start(common::app(CANARY_THEN_REST, 1)).await;
    let v1 = world.source_revision();
    world.spawn(common::host("canary-1").label("canary", "true"));
    world.spawn(common::host("web-1"));
    world.spawn(common::host("web-2"));
    world.converge(&v1, &["canary-1", "web-1", "web-2"]).await;

    let v2 = world.publish(&common::app(CANARY_THEN_REST, 2)).await.id;
    let rollout = world.rollout_for(&v2).await;
    let id = rollout.summary.id.clone();

    // Hosts are frozen into the first phase that matches; the canary waits for sign-off.
    let detail = world
        .wait_rollout(&id, "the canary awaiting approval", |d| {
            d.summary.state == RolloutState::AwaitingApproval
                && rollout_host(d, "canary-1").is_some_and(|h| h.needs_approval)
        })
        .await;
    let canary = phase(&detail, "canary").unwrap();
    assert_eq!(canary.state, PhaseState::AwaitingApproval);
    assert_eq!(
        canary
            .hosts
            .iter()
            .map(|h| h.host.as_str())
            .collect::<Vec<_>>(),
        ["canary-1"]
    );
    let rest = phase(&detail, "rest").unwrap();
    let mut rest_hosts: Vec<&str> = rest.hosts.iter().map(|h| h.host.as_str()).collect();
    rest_hosts.sort();
    assert_eq!(rest_hosts, ["web-1", "web-2"]);
    assert_eq!(rest.state, PhaseState::Pending);
    for h in ["canary-1", "web-1", "web-2"] {
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(1)),
            "{h}"
        );
    }

    // Approving the canary phase applies the canary and nothing else.
    world.approve(&id, "canary").await;
    let detail = world
        .wait_rollout_host(&id, "canary-1", HostRolloutState::Baking)
        .await;
    assert_eq!(
        world.agent("canary-1").file(APP_CONF),
        Some(common::app_conf(2))
    );
    for h in ["web-1", "web-2"] {
        assert_eq!(
            rollout_host(&detail, h).unwrap().state,
            HostRolloutState::Planned,
            "{h} waits for the canary to bake"
        );
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(1)),
            "{h}"
        );
    }
    let canary = phase(&detail, "canary").unwrap();
    assert_eq!(canary.approved_by.as_deref(), Some("anonymous"));

    // Hosts enrolled after the freeze: web-3's phase has not started, so it
    // joins it; canary-2's phase is running, so it is left for the stragglers.
    world.spawn(common::host("web-3"));
    world.spawn(common::host("canary-2").label("canary", "true"));
    let detail = world
        .wait_rollout(&id, "web-3 admitted to the rest phase", |d| {
            rollout_host(d, "web-3").is_some_and(|h| h.phase == "rest")
        })
        .await;
    assert!(rollout_host(&detail, "canary-2").is_none());

    // The canary bakes, the gate passes, the rest phase runs and the revision is promoted.
    let detail = world
        .wait_rollout(&id, "completed", |d| {
            d.summary.state == RolloutState::Completed
        })
        .await;
    assert_eq!(phase(&detail, "canary").unwrap().state, PhaseState::Done);
    assert_eq!(phase(&detail, "rest").unwrap().state, PhaseState::Done);
    for h in ["canary-1", "web-1", "web-2", "web-3"] {
        let row = rollout_host(&detail, h).unwrap();
        assert_eq!(row.state, HostRolloutState::Healthy, "{h}");
        assert!(row.wave.is_some(), "{h} ran in a wave");
    }
    assert_eq!(rollout_host(&detail, "web-3").unwrap().phase, "rest");
    let fleet = world
        .wait_fleet("v2 to be stable", |f| {
            f.stable_revision.as_deref() == Some(v2.as_str())
        })
        .await;
    assert_eq!(fleet.active_rollout, None);
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    for kind in [
        "rollout.approve",
        "rollout.target.admitted",
        "rollout.phase.done",
        "revision.promoted",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }

    // Promotion opened the stragglers stage, which takes the host that missed
    // its phase to the new stable revision.
    let detail = world
        .wait_rollout(&id, "canary-2 healthy in the stragglers stage", |d| {
            phase(d, "stragglers").is_some_and(|p| {
                p.virtual_stage
                    && p.hosts
                        .iter()
                        .any(|h| h.host == "canary-2" && h.state == HostRolloutState::Healthy)
            })
        })
        .await;
    assert_eq!(
        phase(&detail, "stragglers").unwrap().state,
        PhaseState::Open
    );
    world
        .wait_host("canary-2", "on v2", |h| {
            h.applied_revision.as_deref() == Some(v2.as_str())
        })
        .await;
    for h in ["canary-1", "web-1", "web-2", "web-3", "canary-2"] {
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(2)),
            "{h}"
        );
    }
    world.shutdown().await;
}
