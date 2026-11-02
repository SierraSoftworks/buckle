//! Scenario: a host's run report during a rollout. While a slow step runs,
//! the report shows the module and that step `running` with the steps before
//! it `changed`, and the live stream carries `run_progress` events; after a
//! later step fails, the report shows it `failed` with a structured error
//! (message and advice, no dumped text) and the steps after it `not_run`.
//! The host's own run endpoint and `HostDetail.latest_run` serve the same
//! final report.

use crate::common::{self, APP_CONF};
use buckle_api::{HostDetail, HostRolloutState, ModuleState, RunReport, RunState, StepState};
use buckle_core::system::{Command, Output};
use buckle_core::testing::{FakeSystem, ok_output};
use buckle_integration::{Fleet, WAIT, World, api::Sse, eventually, rollout_host};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Revision 2 of `app`: the new file, a slow step, a failing step and one
/// after it.
fn v2() -> Fleet {
    Fleet::with_phases(
        "  health: { baseline: skip, bake: 1s }\n",
        common::ONE_PHASE,
    )
    .module(
        "app",
        "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\n  - command.run: { run: [slow-step] }\n  - command.run: { run: [break-app] }\n  - command.run: { run: [after-step] }\n",
    )
    .file("app", "app.conf", common::app_conf(2))
}

/// `slow-step` waits until `release` is set; `break-app` exits 1.
fn scripted(system: FakeSystem, release: Arc<AtomicBool>) -> FakeSystem {
    system
        .on_command("slow-step", &[], move |_: &Command| {
            tokio::task::block_in_place(|| {
                let deadline = std::time::Instant::now() + WAIT;
                while !release.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            ok_output("slow step done\n")
        })
        .on_command("break-app", &[], |_: &Command| Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"break-app: the socket is gone\n".to_vec(),
            timed_out: false,
        })
        .on_command_ok("after-step", &[], "")
}

fn step<'a>(report: &'a RunReport, needle: &str) -> &'a buckle_api::StepReport {
    report
        .modules
        .iter()
        .flat_map(|m| &m.steps)
        .find(|s| s.id.contains(needle))
        .unwrap_or_else(|| panic!("no step {needle} in {report:#?}"))
}

fn app(report: &RunReport) -> &buckle_api::ModuleReport {
    report
        .modules
        .iter()
        .find(|m| m.name == "app")
        .unwrap_or_else(|| panic!("no app module in {report:#?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosts_run_report_follows_the_rollout_live_and_ends_with_a_structured_error() {
    let fleet = common::app_module(
        Fleet::with_phases(
            "  health: { baseline: skip, bake: 1s }\n",
            common::ONE_PHASE,
        ),
        1,
    );
    let mut world = World::start(fleet).await;
    let v1 = world.source_revision();
    let release = Arc::new(AtomicBool::new(false));
    let agent = world.spawn(
        common::host("rep-1").system(scripted(common::healthy_system("rep-1"), release.clone())),
    );
    world.converge(&v1, &["rep-1"]).await;

    let sse = Sse::connect(world.api()).await;
    let v2 = world.publish(&v2()).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    let report_path = format!("/api/v1/rollouts/{id}/hosts/rep-1/report");

    // While the slow step runs: it and its module are running, the step
    // before it changed, the ones after it are still pending.
    let live: RunReport = eventually("the slow step running in the report", WAIT, async || {
        let report: Option<RunReport> = world.api().try_get(&report_path).await;
        let report = report.ok_or("no report yet")?;
        let ok = report.state == RunState::Running
            && report
                .modules
                .iter()
                .any(|m| m.name == "app" && m.state == ModuleState::Running)
            && report
                .modules
                .iter()
                .flat_map(|m| &m.steps)
                .any(|s| s.id.contains("slow-step") && s.state == StepState::Running);
        if ok {
            Ok(report)
        } else {
            Err(format!("{report:#?}"))
        }
    })
    .await;
    assert_eq!(
        step(&live, "app.conf").state,
        StepState::Changed,
        "{live:#?}"
    );
    assert_eq!(step(&live, "break-app").state, StepState::Pending);
    assert_eq!(step(&live, "after-step").state, StepState::Pending);
    assert!(!live.id.is_empty(), "a live run has its id");
    assert_eq!(live.rollout_id.as_deref(), Some(id.as_str()));

    let run_id = live.id.clone();
    eventually("run_progress on the live stream", WAIT, async || {
        let raw = sse.raw();
        if raw.contains("event: run_progress") && raw.contains(&run_id) {
            Ok(())
        } else {
            Err(raw)
        }
    })
    .await;
    let rollout = world
        .api()
        .get::<buckle_api::RolloutDetail>(&format!("/api/v1/rollouts/{id}"))
        .await;
    assert_eq!(
        rollout_host(&rollout, "rep-1").and_then(|h| h.run_id.clone()),
        Some(run_id.clone()),
        "the rollout host names its run"
    );

    // Let the slow step finish: the next step fails.
    release.store(true, Ordering::SeqCst);
    world
        .wait_rollout(&id, "rep-1 failed", |d| {
            rollout_host(d, "rep-1").is_some_and(|h| h.state == HostRolloutState::Failed)
        })
        .await;
    let done: RunReport = eventually("the final report", WAIT, async || {
        let report: RunReport = world.api().get(&report_path).await;
        if report.finished_at.is_some() {
            Ok(report)
        } else {
            Err(format!("{report:#?}"))
        }
    })
    .await;
    assert_eq!(done.id, run_id, "the live run became the final one");
    assert_eq!(done.rollout_id.as_deref(), Some(id.as_str()));
    let progress: Vec<buckle_api::RunProgress> =
        eventually("the final run_progress", WAIT, async || {
            let progress: Vec<_> = sse
                .events()
                .into_iter()
                .filter_map(|e| match e {
                    buckle_api::LiveEvent::RunProgress { progress } if progress.host == "rep-1" => {
                        Some(progress)
                    }
                    _ => None,
                })
                .collect();
            if progress.iter().any(|p| p.run_state == RunState::Failed) {
                Ok(progress)
            } else {
                Err(format!("{progress:#?}"))
            }
        })
        .await;
    assert!(
        progress
            .iter()
            .all(|p| p.run_id == run_id && p.rollout_id.as_deref() == Some(id.as_str())),
        "every update of a rollout's run names the rollout: {progress:#?}"
    );
    assert!(
        progress.iter().any(
            |p| p.step.as_deref().is_some_and(|s| s.contains("slow-step"))
                && p.step_state == Some(StepState::Running)
        ),
        "{progress:#?}"
    );
    assert_eq!(done.state, RunState::Failed);
    assert!(
        matches!(
            app(&done).state,
            ModuleState::Failed | ModuleState::RolledBack
        ),
        "{:?}",
        app(&done).state
    );
    assert_eq!(
        step(&done, "slow-step")
            .result
            .as_deref()
            .map(|r| !r.is_empty()),
        Some(true)
    );
    let failed = step(&done, "break-app");
    assert_eq!(failed.state, StepState::Failed, "{done:#?}");
    let error = failed.error.as_ref().expect("a structured error");
    assert!(error.message.contains("break-app"), "{error:?}");
    assert!(!error.advice.is_empty(), "{error:?}");
    for text in std::iter::once(&error.message).chain(&error.causes) {
        assert!(!text.contains("To try and fix this"), "no dump: {text}");
        assert!(!text.contains("This was caused by"), "no dump: {text}");
    }
    assert_eq!(step(&done, "after-step").state, StepState::NotRun);
    assert!(done.counts.steps.failed >= 1 && done.counts.steps.not_run >= 1);
    let row = rollout_host(
        &world.api().get(&format!("/api/v1/rollouts/{id}")).await,
        "rep-1",
    )
    .cloned()
    .unwrap();
    assert!(
        row.error_detail
            .as_ref()
            .is_some_and(|e| !e.advice.is_empty()),
        "{row:#?}"
    );

    // The host's own endpoints serve the same run.
    let by_host: RunReport = world
        .api()
        .get(&format!("/api/v1/hosts/rep-1/runs/{run_id}"))
        .await;
    assert_eq!(by_host.id, done.id);
    assert_eq!(by_host.state, done.state);
    assert_eq!(by_host.modules.len(), done.modules.len());
    for (a, b) in by_host.modules.iter().zip(&done.modules) {
        assert_eq!((&a.name, a.state, &a.steps), (&b.name, b.state, &b.steps));
    }
    let host: HostDetail = world.api().get("/api/v1/hosts/rep-1").await;
    let latest = host.latest_run.expect("the latest run");
    assert_eq!(latest.id, run_id);
    assert_eq!(latest.state, RunState::Failed);
    assert_eq!(
        host.recent_runs.first().map(|r| (r.id.clone(), r.state)),
        Some((run_id.clone(), Some(RunState::Failed)))
    );
    assert_eq!(
        agent.file(APP_CONF),
        Some(common::app_conf(1)),
        "the failed module was rolled back"
    );
    drop(sse);
    world.shutdown().await;
}
