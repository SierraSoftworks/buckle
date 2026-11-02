//! Scenario 8: drift (design §04 "Drift", §07 "With each drift check"). A
//! hand edit to a managed file shows up in the next drift pass as a plan
//! against the recorded state; with `drift.correct` the agent applies it,
//! otherwise the host is reported drifted. Each pass also runs the applied
//! modules' checks and reports them with the drift scope.

use crate::common::{self, APP_CONF, ONE_PHASE};
use buckle_agent::transport::Report;
use buckle_api::{FleetCounts, HealthResultDto};
use buckle_integration::{Fleet, WAIT, World, eventually};
use rstest::rstest;
use std::time::Duration;

const HAND_EDIT: &str = "edited by hand on the host\n";

#[rstest]
#[case::reported(false)]
#[case::corrected(true)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_edit_is_reported_as_drift_and_corrected_when_configured(#[case] correct: bool) {
    let mut world = World::start(common::checked_app_module(
        Fleet::with_phases("", ONE_PHASE),
        1,
    ))
    .await;
    let v1 = world.source_revision();
    let host = world.spawn(common::host("drift-1").drift(Duration::from_secs(1), correct));
    world.converge(&v1, &["drift-1"]).await;
    let before = host.reports().len();

    host.write_file(APP_CONF, HAND_EDIT);

    // The next pass re-plans the applied revision and finds the edit.
    let (plan, corrected) = eventually("a drift report with a plan", WAIT, async || {
        host.reports()[before..]
            .iter()
            .find_map(|r| match r {
                Report::Drift {
                    revision,
                    plan: Some(plan),
                    corrected,
                } if revision.0 == v1 => Some((plan.clone(), *corrected)),
                _ => None,
            })
            .ok_or("no drift yet".to_owned())
    })
    .await;
    assert_eq!(corrected, correct);
    assert_eq!(plan.changes(), 1);
    let shown = serde_json::to_string(&plan).unwrap();
    assert!(
        shown.contains("edited by hand"),
        "the drift plan shows what the hand edit was: {shown}"
    );

    if correct {
        assert_eq!(host.file(APP_CONF), Some(common::app_conf(1)), "corrected");
        world
            .wait_host("drift-1", "not drifted", |h| !h.drifted)
            .await;
    } else {
        world.wait_host("drift-1", "drifted", |h| h.drifted).await;
        let counts: FleetCounts = world
            .api()
            .get::<buckle_api::Fleet>("/api/v1/fleet")
            .await
            .counts;
        assert_eq!(counts.drifted, 1);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            host.file(APP_CONF),
            Some(HAND_EDIT.to_owned()),
            "left alone"
        );
    }

    // The drift pass's health results are the host's between-rollout health.
    let health = eventually("drift-scope health results", WAIT, async || {
        let results: Vec<HealthResultDto> = world
            .api()
            .try_get("/api/v1/hosts/drift-1/health")
            .await
            .ok_or("unavailable")?;
        results
            .iter()
            .any(|r| r.scope == "drift")
            .then_some(results.clone())
            .ok_or(format!("{results:?}"))
    })
    .await;
    let drift: Vec<&HealthResultDto> = health.iter().filter(|r| r.scope == "drift").collect();
    assert!(
        drift
            .iter()
            .all(|r| r.module == "app" && r.check == "healthy" && r.status == "pass"),
        "{drift:?}"
    );
    world.shutdown().await;
}
