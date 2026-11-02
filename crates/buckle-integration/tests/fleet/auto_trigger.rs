//! Scenario 2: a commit to the source becomes a validated revision, an
//! automatic rollout and plans on every host, grouped by digest for review
//! (design §09, §10 "Plan everywhere first", §11 "Dedupe by digest").

use crate::common::{self, APP_CONF};
use buckle_api::{HostRolloutState, LiveEvent, PlanGroup, RevisionStatus, RolloutState};
use buckle_integration::{World, rollout_host};

/// One phase that waits for sign-off, so the plans can be inspected before
/// anything is applied.
const REVIEWED: &str =
    "    - { name: everyone, select: 'true', parallel: 10, approval: required, bake: 1s }\n";

const DB_MODULE: &str = "name: db\nwhen: 'host.labels.role == \"db\"'\ntasks:\n  - file.copy: { src: db.conf, dest: /etc/db.conf }\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_revision_is_validated_rolled_out_and_planned_everywhere_with_plans_grouped_by_digest()
 {
    let mut world = World::start(common::app(REVIEWED, 1)).await;
    let v1 = world.source_revision();
    for (id, role) in [("web-1", "web"), ("web-2", "web"), ("db-1", "db")] {
        world.spawn(common::host(id).label("role", role));
    }
    world.converge(&v1, &["web-1", "web-2", "db-1"]).await;
    let sse = world.sse().await;

    // A new commit: app.conf changes everywhere, and a module only db hosts select.
    let v2_fleet = common::app(REVIEWED, 2).module("db", DB_MODULE).file(
        "db",
        "db.conf",
        "max_connections = 100\n",
    );
    let v2 = world.publish(&v2_fleet).await;
    assert_eq!(
        v2.status,
        RevisionStatus::Valid,
        "{:?}",
        v2.validation_errors
    );
    assert_eq!(v2.module_count, 2);

    // The revision triggered a rollout on its own, which asked every host for a plan.
    let rollout = world.rollout_for(&v2.id).await;
    assert_eq!(rollout.summary.triggered_by, "controller");
    let rollout = world
        .wait_rollout(&rollout.summary.id, "every host planned", |d| {
            d.summary.state == RolloutState::AwaitingApproval
                && ["web-1", "web-2", "db-1"].iter().all(|h| {
                    rollout_host(d, h).is_some_and(|r| {
                        r.state == HostRolloutState::Planned && r.plan_digest.is_some()
                    })
                })
        })
        .await;
    for h in ["web-1", "web-2", "db-1"] {
        assert!(rollout_host(&rollout, h).unwrap().needs_approval, "{h}");
    }

    // Identical plans are one review: two groups for three hosts.
    let mut groups: Vec<PlanGroup> = world
        .api()
        .get(&format!("/api/v1/rollouts/{}/plans", rollout.summary.id))
        .await;
    groups.sort_by_key(|g| g.hosts.len());
    assert_eq!(groups.len(), 2, "{groups:#?}");
    assert_eq!(groups[0].hosts, vec!["db-1".to_owned()]);
    assert_eq!(groups[0].changes, 2);
    let mut web: Vec<String> = groups[1].hosts.clone();
    web.sort();
    assert_eq!(web, vec!["web-1".to_owned(), "web-2".to_owned()]);
    assert_eq!(groups[1].changes, 1);
    let shared = serde_json::to_string(&groups[1].plan).unwrap();
    assert!(
        shared.contains("app revision 2"),
        "the shared plan shows the new content: {shared}"
    );
    assert!(!shared.contains("/etc/db.conf"));
    assert!(
        serde_json::to_string(&groups[0].plan)
            .unwrap()
            .contains("/etc/db.conf")
    );
    assert_eq!(
        rollout_host(&rollout, "web-1").unwrap().plan_digest,
        rollout_host(&rollout, "web-2").unwrap().plan_digest
    );

    // Planning is pure: nothing changed on any host while waiting for sign-off.
    for h in ["web-1", "web-2", "db-1"] {
        assert_eq!(
            world.agent(h).file(APP_CONF),
            Some(common::app_conf(1)),
            "{h}"
        );
    }
    assert_eq!(world.agent("db-1").file("/etc/db.conf"), None);

    // A broken commit is stored as rejected, with its errors, and starts nothing.
    let broken = world
        .publish(&v2_fleet.clone().file("app", "broken.j2", "{% if %}"))
        .await;
    assert_eq!(broken.status, RevisionStatus::Rejected);
    assert!(
        broken
            .validation_errors
            .iter()
            .any(|e| e.contains("broken.j2")),
        "{:?}",
        broken.validation_errors
    );
    let still = world
        .api()
        .get::<buckle_api::RolloutDetail>(&format!("/api/v1/rollouts/{}", rollout.summary.id))
        .await;
    assert_eq!(still.summary.state, RolloutState::AwaitingApproval);
    let rollouts: buckle_api::Page<buckle_api::RolloutSummary> =
        world.api().get("/api/v1/rollouts").await;
    assert!(rollouts.items.iter().all(|r| r.revision != broken.id));

    // Operators watching the event stream saw the revision and the rollout arrive.
    let events = sse.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LiveEvent::RevisionAdded { revision } if revision.id == v2.id)),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        LiveEvent::RolloutChanged { rollout: r }
            if r.id == rollout.summary.id && r.state == RolloutState::AwaitingApproval
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        LiveEvent::RolloutHostChanged { rollout_id, host }
            if rollout_id == &rollout.summary.id && host.state == HostRolloutState::Planned
    )));
    sse.close();
    world.shutdown().await;
}
