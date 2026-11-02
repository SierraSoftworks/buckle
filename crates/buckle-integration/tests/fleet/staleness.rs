//! Scenario 9: an approval binds what the plan does (design §08 "Assignment is
//! not apply", §11 "Approval binds digests"). When the host changes between
//! its plan and the approved Apply so that the fresh plan acts differently,
//! the agent refuses to apply and the host needs approval again; when only
//! the content changed, the approved actions run from the fresh plan.

use crate::common::{self, APP_CONF, CANARY_THEN_REST};
use buckle_agent::transport::Report;
use buckle_api::{HostRolloutState, RolloutState};
use buckle_integration::{World, rollout_host};
use reqwest::StatusCode;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_changed_after_approval_refuses_the_stale_plan_and_needs_approval_again() {
    let mut world = World::start(common::app(CANARY_THEN_REST, 1)).await;
    let v1 = world.source_revision();
    let canary = world.spawn(common::host("canary-1").label("canary", "true"));
    world.converge(&v1, &["canary-1"]).await;

    let v2 = world.publish(&common::app(CANARY_THEN_REST, 2)).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    let detail = world
        .wait_rollout(&id, "the canary planned", |d| {
            rollout_host(d, "canary-1").is_some_and(|h| h.needs_approval)
        })
        .await;
    let reviewed = rollout_host(&detail, "canary-1")
        .and_then(|h| h.plan_digest.clone())
        .unwrap();

    // Someone deletes the file after the operator reviewed its update: the
    // fresh plan creates it instead.
    canary.remove_file(APP_CONF).await;
    let approved = world.approve(&id, "canary").await;
    assert_eq!(approved, vec![reviewed.clone()]);

    // The agent re-plans before applying, finds a different plan and refuses.
    let detail = world
        .wait_rollout(&id, "the canary back to needing approval", |d| {
            d.summary.state == RolloutState::AwaitingApproval
                && rollout_host(d, "canary-1").is_some_and(|h| {
                    h.state == HostRolloutState::Planned
                        && h.needs_approval
                        && h.plan_digest.as_deref().is_some_and(|p| p != reviewed)
                })
        })
        .await;
    let fresh = rollout_host(&detail, "canary-1")
        .and_then(|h| h.plan_digest.clone())
        .unwrap();
    assert_eq!(
        canary.file(APP_CONF),
        None,
        "the stale plan was not applied"
    );
    let refused = canary.reports().iter().any(|r| {
        matches!(r, Report::StalePlan { plan, approved_digest }
            if approved_digest == &reviewed && plan.digest.as_str() == fresh)
    });
    assert!(
        refused,
        "the agent reported the stale plan with its fresh digest"
    );

    // The old approval does not cover the new plan.
    let (status, body) = world
        .api()
        .post(
            &format!("/api/v1/rollouts/{id}/approve"),
            serde_json::json!({ "phase": "canary", "plan_digests": [reviewed] }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Approving what the host now plans applies it.
    assert_eq!(world.approve(&id, "canary").await, vec![fresh]);
    world
        .wait_rollout(&id, "completed", |d| {
            d.summary.state == RolloutState::Completed
        })
        .await;
    assert_eq!(canary.file(APP_CONF), Some(common::app_conf(2)));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_content_only_change_after_approval_applies_under_the_same_approval() {
    let mut world = World::start(common::app(CANARY_THEN_REST, 1)).await;
    let v1 = world.source_revision();
    let canary = world.spawn(common::host("canary-1").label("canary", "true"));
    world.converge(&v1, &["canary-1"]).await;

    let v2 = world.publish(&common::app(CANARY_THEN_REST, 2)).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    world
        .wait_rollout(&id, "the canary planned", |d| {
            rollout_host(d, "canary-1").is_some_and(|h| h.needs_approval)
        })
        .await;

    // Someone edits the file after review: the update now has another diff,
    // but it is still the update that was approved.
    canary.write_file(APP_CONF, "edited after review\n");
    world.approve(&id, "canary").await;
    let detail = world
        .wait_rollout(&id, "completed", |d| {
            d.summary.state == RolloutState::Completed
        })
        .await;
    assert_eq!(canary.file(APP_CONF), Some(common::app_conf(2)));
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == "rollout.host.plan_content_changed"),
        "the content change is recorded"
    );
    world.shutdown().await;
}
