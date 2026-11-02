//! Scenario 5: health gates the rollout (design §07). A host already failing
//! its checks before the change is handled per `health.baseline`; a check that
//! starts failing during bake and stays failing past `settle` fails the host
//! and counts toward `max_failures`, while one that recovers inside `settle`
//! is a flap.

use crate::common::{self, APP_CONF, HEALTHY};
use buckle_agent::transport::Report;
use buckle_api::{HealthResultDto, HostRolloutState, RolloutState};
use buckle_core::health::HealthScope;
use buckle_integration::{Fleet, WAIT, World, eventually, rollout_host};
use rstest::rstest;

/// The checked `app` at revision `n`; from revision 2 it also writes the
/// health marker, so the change itself repairs a sick host.
fn fleet(baseline: &str, settle: &str, bake: &str, n: u32) -> Fleet {
    let phases = format!(
        "    - {{ name: fleet, select: 'true', parallel: 10, approval: auto, bake: {bake}, max_failures: 0 }}\n"
    );
    let fleet = common::checked_app_module(
        Fleet::with_phases(
            &format!("  health: {{ bake: 1s, baseline: {baseline} }}\n  settle: {settle}\n"),
            &phases,
        ),
        n,
    );
    if n < 2 {
        return fleet;
    }
    fleet
        .module(
            "app",
            "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\n  - file.copy: { src: marker, dest: /run/app.healthy }\nhealth:\n  - name: healthy\n    interval: 1s\n    file: { path: /run/app.healthy }\n",
        )
        .file("app", "marker", "ok\n")
}

#[rstest]
#[case::hold("hold")]
#[case::skip("skip")]
#[case::ignore("ignore")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_unhealthy_before_the_change_is_handled_per_the_baseline_policy(
    #[case] baseline: &str,
) {
    let mut world = World::start(fleet(baseline, "1s", "1s", 1)).await;
    let v1 = world.source_revision();
    let sick = world.spawn(common::host("sick-1"));
    world.spawn(common::host("ok-1"));
    world.converge(&v1, &["sick-1", "ok-1"]).await;

    // The host breaks before the next rollout.
    sick.remove_file(HEALTHY).await;
    let v2 = world.publish(&fleet(baseline, "1s", "1s", 2)).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    world
        .wait_rollout_host(&id, "ok-1", HostRolloutState::Healthy)
        .await;

    match baseline {
        "hold" => {
            // Held: the wave went on without it, and nothing was applied.
            let detail = world
                .wait_rollout_host(&id, "sick-1", HostRolloutState::Held)
                .await;
            let row = rollout_host(&detail, "sick-1").unwrap();
            assert_eq!(row.failed_checks, vec!["app/healthy".to_owned()]);
            assert!(
                row.error
                    .as_deref()
                    .is_some_and(|e| e.contains("baseline failing")),
                "{:?}",
                row.error
            );
            assert_eq!(sick.file(APP_CONF), Some(common::app_conf(1)));
            let health: Vec<HealthResultDto> = world.api().get("/api/v1/hosts/sick-1/health").await;
            assert!(
                health
                    .iter()
                    .any(|h| h.scope == "baseline" && h.check == "healthy" && h.status == "fail"),
                "{health:?}"
            );
            // The operator forces it; the change repairs it and it completes.
            world
                .api()
                .post_ok(
                    &format!("/api/v1/rollouts/{id}/hosts/sick-1/force"),
                    serde_json::json!({ "note": "the new revision restores the marker" }),
                )
                .await;
            let detail = world
                .wait_rollout(&id, "completed", |d| {
                    d.summary.state == RolloutState::Completed
                })
                .await;
            assert_eq!(
                rollout_host(&detail, "sick-1").unwrap().state,
                HostRolloutState::Healthy
            );
            assert!(detail.events.iter().any(|e| e.kind == "rollout.host.force"));
            assert_eq!(sick.file(APP_CONF), Some(common::app_conf(2)));
            let forced = sick.reports().iter().any(|r| {
                matches!(r, Report::ApplyResult { revision, outcome, .. } if revision.0 == v2 && outcome.succeeded)
            });
            assert!(forced);
        }
        "skip" => {
            let detail = world
                .wait_rollout(&id, "completed without the sick host", |d| {
                    d.summary.state == RolloutState::Completed
                })
                .await;
            assert_eq!(
                rollout_host(&detail, "sick-1").unwrap().state,
                HostRolloutState::Skipped
            );
            assert_eq!(sick.file(APP_CONF), Some(common::app_conf(1)));
        }
        "ignore" => {
            let detail = world
                .wait_rollout(&id, "completed", |d| {
                    d.summary.state == RolloutState::Completed
                })
                .await;
            assert_eq!(
                rollout_host(&detail, "sick-1").unwrap().state,
                HostRolloutState::Healthy
            );
            assert_eq!(sick.file(APP_CONF), Some(common::app_conf(2)));
        }
        other => unreachable!("{other}"),
    }
    world.shutdown().await;
}

/// Whether the agent has sent a bake report in which `app/healthy` failed.
fn reported_failing_bake(reports: &[Report]) -> bool {
    reports.iter().any(|r| {
        matches!(r, Report::Health { report, .. }
            if report.scope == HealthScope::Bake
                && report.results.iter().any(|x| x.check == "healthy" && !x.status.is_pass()))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_check_failing_through_settle_during_bake_fails_the_host_and_breaches_the_budget_while_a_flap_does_not()
 {
    let mut world = World::start(fleet("hold", "2s", "3s", 1)).await;
    let v1 = world.source_revision();
    let flaky = world.spawn(common::host("flaky-1"));
    let flap = world.spawn(common::host("flap-1"));
    world.converge(&v1, &["flaky-1", "flap-1"]).await;

    let v2 = world
        .publish(&fleet("hold", "2s", "3s", 1).file("app", "app.conf", common::app_conf(2)))
        .await
        .id;
    let id = world.rollout_for(&v2).await.summary.id;
    world
        .wait_rollout(&id, "both hosts baking", |d| {
            ["flaky-1", "flap-1"]
                .iter()
                .all(|h| rollout_host(d, h).is_some_and(|r| r.state == HostRolloutState::Baking))
        })
        .await;

    // Both checks start failing during bake; one recovers within settle.
    flaky.remove_file(HEALTHY).await;
    flap.remove_file(HEALTHY).await;
    eventually("flap-1 to report a failing bake check", WAIT, async || {
        reported_failing_bake(&flap.reports())
            .then_some(())
            .ok_or("not yet".to_owned())
    })
    .await;
    flap.write_file(HEALTHY, "ok\n");

    let detail = world
        .wait_rollout(&id, "paused on the bake regression", |d| {
            d.summary.state == RolloutState::Paused
                && rollout_host(d, "flaky-1").is_some_and(|r| r.state == HostRolloutState::Failed)
                && rollout_host(d, "flap-1").is_some_and(|r| r.state == HostRolloutState::Healthy)
        })
        .await;
    let row = rollout_host(&detail, "flaky-1").unwrap();
    assert_eq!(row.failed_checks, vec!["app/healthy".to_owned()]);
    // The server's settle timer and the agent's module rollback (design
    // §04) race to fail the host; either names the failure.
    assert!(
        row.error
            .as_deref()
            .is_some_and(|e| e.contains("bake regression") || e.contains("rolled module(s) app")),
        "{:?}",
        row.error
    );
    assert_eq!(detail.summary.hosts_failed, 1);
    assert!(
        detail
            .summary
            .pause_reason
            .as_deref()
            .is_some_and(|r| r.contains("flaky-1")),
        "{:?}",
        detail.summary.pause_reason
    );
    assert_eq!(rollout_host(&detail, "flap-1").unwrap().error, None);
    // The apply itself succeeded on both hosts; on flaky-1 the failing
    // module was then rolled back to the stable revision on its own.
    assert_eq!(flap.file(APP_CONF), Some(common::app_conf(2)));
    eventually("flaky-1 to roll its app module back", WAIT, async || {
        (flaky.file(APP_CONF) == Some(common::app_conf(1)))
            .then_some(())
            .ok_or(format!("{:?}", flaky.file(APP_CONF)))
    })
    .await;
    world.shutdown().await;
}
