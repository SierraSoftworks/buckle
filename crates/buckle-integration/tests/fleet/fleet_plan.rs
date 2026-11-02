//! The rollout's union plan: a change that differs between hosts only by
//! a host's own values reads as one change, with fact placeholders.

use crate::common::{self, ONE_PHASE};
use buckle_integration::{Fleet, World, rollout_host};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_that_differs_only_by_host_name_is_one_fleet_wide_change() {
    let fleet = |template: &str, approval: &str| {
        let base = Fleet::with_phases("", &ONE_PHASE.replace("approval: auto", approval));
        base.module(
            "svc",
            "name: svc\ntasks:\n  - file.template: { src: svc.j2, dest: /etc/svc.conf }\n",
        )
        .file("svc", "svc.j2", template)
    };
    let mut world = World::start(fleet("name = old\n", "approval: auto")).await;
    let v1 = world.source_revision();
    for h in ["svc-1", "svc-2"] {
        world.spawn(common::host(h));
    }
    world.converge(&v1, &["svc-1", "svc-2"]).await;
    let v2 = world
        .publish(&fleet("name = {{ host.id }}\n", "approval: required"))
        .await
        .id;
    let id = world.rollout_for(&v2).await.summary.id;
    let detail = world
        .wait_rollout(&id, "planned", |d| {
            ["svc-1", "svc-2"]
                .iter()
                .all(|h| rollout_host(d, h).is_some_and(|h| h.needs_approval))
        })
        .await;

    let fleet = detail.fleet.expect("a fleet plan");
    let step = fleet
        .modules
        .iter()
        .flat_map(|m| &m.steps)
        .find(|s| s.key == "/etc/svc.conf")
        .expect("the svc.conf step");
    assert_eq!(step.variants.len(), 1, "{step:#?}");
    assert!(fleet.is_fleet_wide(&step.variants[0].hosts));
    let unified = step.variants[0]
        .value
        .unified
        .as_deref()
        .unwrap_or_default();
    assert!(unified.contains("+name = «fact:host.id»"), "{unified}");
    let samples = fleet.fact("host.id").expect("host.id exemplars");
    assert_eq!(samples.distinct, 2);
    world.shutdown().await;
}
