//! Scenario 4: a failing module is rolled back to the stable revision on the
//! host, the failure budget pauses the rollout, and the operator's decisions
//! (retry, continue with waived checks, skip) behave as design §04
//! "Failure handling" and §10 "Paused offers four operator decisions" say.

use crate::common::{self, APP_CONF, HEALTHY};
use buckle_agent::transport::Report;
use buckle_api::{HostRolloutState, RolloutState};
use buckle_core::system::{Command, FilePerms, Output};
use buckle_core::testing::{FakeSystem, ok_output};
use buckle_integration::{Fleet, World, rollout_host};
use reqwest::StatusCode;

const DEPLOYED: &str = "/etc/app/deployed";

/// Revision 2 of `app`: the new file, a deploy command guarded by `creates`,
/// and a health check on the marker.
fn v2() -> Fleet {
    Fleet::with_phases(
        "",
        "    - { name: fleet, select: 'true', parallel: 10, approval: auto, bake: 1s, max_failures: 0 }\n",
    )
    .module(
        "app",
        "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\n  - command.run: { run: [deploy-app], creates: /etc/app/deployed }\nhealth:\n  - name: healthy\n    interval: 1s\n    file: { path: /run/app.healthy }\n",
    )
    .file("app", "app.conf", common::app_conf(2))
}

/// `deploy-app` succeeds and leaves its marker.
fn deploys(system: FakeSystem) -> FakeSystem {
    let marker = system.clone();
    system.on_command("deploy-app", &[], move |_: &Command| {
        let _ = marker
            .clone()
            .with_file(DEPLOYED, "yes\n", FilePerms::mode(0o644));
        ok_output("deployed\n")
    })
}

/// `deploy-app` exits 1.
fn fails_to_deploy(system: FakeSystem) -> FakeSystem {
    system.on_command("deploy-app", &[], |_: &Command| Output {
        code: Some(1),
        stdout: vec![],
        stderr: b"deploy-app: connection refused\n".to_vec(),
        timed_out: false,
    })
}

fn deploy_runs(system: &FakeSystem) -> usize {
    system
        .command_lines()
        .iter()
        .filter(|l| l.starts_with("deploy-app"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_modules_roll_back_locally_the_budget_pauses_and_operator_decisions_resolve_it() {
    let mut world = World::start(common::app(common::ONE_PHASE, 1)).await;
    let v1 = world.source_revision();
    let ok = world.spawn(common::host("ok-1").system(deploys(common::healthy_system("ok-1"))));
    let task = world
        .spawn(common::host("task-1").system(fails_to_deploy(common::healthy_system("task-1"))));
    let skip = world
        .spawn(common::host("skip-1").system(fails_to_deploy(common::healthy_system("skip-1"))));
    // No health marker: the new module's check fails after it applies.
    let sick = world
        .spawn(common::host("sick-1").system(deploys(FakeSystem::new().with_hostname("sick-1"))));
    let all = ["ok-1", "task-1", "skip-1", "sick-1"];
    world.converge(&v1, &all).await;

    let v2 = world.publish(&v2()).await.id;
    let id = world.rollout_for(&v2).await.summary.id;

    // Three hosts fail; the first failure breaches max_failures: 0 and pauses.
    let detail = world
        .wait_rollout(&id, "paused with three failed hosts", |d| {
            d.summary.state == RolloutState::Paused
                && ["task-1", "skip-1", "sick-1"].iter().all(|h| {
                    rollout_host(d, h).is_some_and(|r| r.state == HostRolloutState::Failed)
                })
                && rollout_host(d, "ok-1").is_some_and(|r| r.state == HostRolloutState::Healthy)
        })
        .await;
    assert!(
        detail
            .summary
            .pause_reason
            .as_deref()
            .is_some_and(|r| r.contains("exceed max_failures 0")),
        "{:?}",
        detail.summary.pause_reason
    );
    assert_eq!(detail.summary.hosts_failed, 3);
    let task_row = rollout_host(&detail, "task-1").unwrap();
    assert_eq!(task_row.failed_module.as_deref(), Some("app"));
    assert!(
        task_row.failed_checks.is_empty(),
        "a task error, not a check"
    );
    assert!(
        task_row
            .error
            .as_deref()
            .is_some_and(|e| e.contains("deploy-app")),
        "{:?}",
        task_row.error
    );
    let sick_row = rollout_host(&detail, "sick-1").unwrap();
    assert_eq!(sick_row.failed_module.as_deref(), Some("app"));
    assert_eq!(sick_row.failed_checks, vec!["app/healthy".to_owned()]);

    // Module-scoped rollback: each failed host re-applied `app` from the
    // stable revision, and said so in its ApplyResult.
    for agent in [&task, &skip, &sick] {
        assert_eq!(
            agent.file(APP_CONF),
            Some(common::app_conf(1)),
            "{}",
            agent.id
        );
        let rolled_back = agent.reports().iter().any(|r| {
            matches!(r, Report::ApplyResult { revision, outcome, .. }
                if revision.0 == v2 && !outcome.succeeded && outcome.rolled_back_modules == ["app"])
        });
        assert!(rolled_back, "{} reported its module rollback", agent.id);
    }
    assert_eq!(ok.file(APP_CONF), Some(common::app_conf(2)));
    assert_eq!(ok.file(DEPLOYED), Some("yes\n".to_owned()));

    // Continue waives health checks; a task error has none, so it is refused.
    let (status, body) = world
        .api()
        .post(
            &format!("/api/v1/rollouts/{id}/hosts/task-1/continue"),
            serde_json::json!({ "waive_checks": ["app/healthy"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Retry re-plans and re-applies the host as it stands; still broken, it
    // fails again and the standing breach pauses the rollout again.
    world
        .api()
        .post_ok(
            &format!("/api/v1/rollouts/{id}/hosts/task-1/retry"),
            serde_json::json!({}),
        )
        .await;
    world
        .wait_rollout(&id, "task-1 retried, failed again and paused", |d| {
            d.summary.state == RolloutState::Paused
                && rollout_host(d, "task-1").is_some_and(|r| r.state == HostRolloutState::Failed)
                && deploy_runs(&task.system) == 2
        })
        .await;

    // Fix task-1's host and retry it, continue sick-1 past its failing check,
    // and skip skip-1 with a reason.
    let _ = deploys(task.system.clone());
    world
        .api()
        .post_ok(
            &format!("/api/v1/rollouts/{id}/hosts/task-1/retry"),
            serde_json::json!({}),
        )
        .await;
    world
        .api()
        .post_ok(
            &format!("/api/v1/rollouts/{id}/hosts/sick-1/continue"),
            serde_json::json!({ "waive_checks": ["app/healthy"], "note": "known flaky marker" }),
        )
        .await;
    world
        .api()
        .post_ok(
            &format!("/api/v1/rollouts/{id}/hosts/skip-1/skip"),
            serde_json::json!({ "note": "decommissioning" }),
        )
        .await;

    let detail = world
        .wait_rollout(&id, "completed", |d| {
            d.summary.state == RolloutState::Completed
        })
        .await;
    let row = |h: &str| rollout_host(&detail, h).unwrap().clone();
    assert_eq!(row("task-1").state, HostRolloutState::Healthy);
    assert_eq!(row("sick-1").state, HostRolloutState::Healthy);
    assert_eq!(row("sick-1").waived_checks, vec!["app/healthy".to_owned()]);
    assert_eq!(row("skip-1").state, HostRolloutState::Skipped);
    assert!(
        row("skip-1")
            .error
            .as_deref()
            .is_some_and(|e| e.contains("decommissioning")),
        "{:?}",
        row("skip-1").error
    );
    assert_eq!(task.file(APP_CONF), Some(common::app_conf(2)));
    assert_eq!(sick.file(APP_CONF), Some(common::app_conf(2)));
    assert_eq!(sick.file(HEALTHY), None, "the waived check still fails");
    assert_eq!(
        skip.file(APP_CONF),
        Some(common::app_conf(1)),
        "skipped hosts stay put"
    );
    let continued = sick.reports().iter().any(|r| {
        matches!(r, Report::ApplyResult { revision, outcome, .. }
            if revision.0 == v2 && outcome.succeeded)
    });
    assert!(continued, "sick-1 applied with the waiver");
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    for kind in [
        "rollout.host.retry",
        "rollout.host.continue",
        "rollout.host.skip",
        "rollout.paused",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    world.shutdown().await;
}
