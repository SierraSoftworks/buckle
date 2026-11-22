//! A suspended module: paused on its host before its `@suspend` runs, left
//! at the revision it had while a rollout passes it by (its checks reading
//! `paused`), and caught up by the agent once its `@resume` has succeeded.

use crate::common::{self, APP_CONF, ONE_PHASE};
use buckle_api::{HealthResultDto, PausedUnit};
use buckle_integration::{Fleet, WAIT, World, eventually};
use std::time::Duration;

const SUSPENDED: &str = "/run/app.suspended";
const RESUMED: &str = "/run/app.resumed";

/// `app` at revision `n`, with hooks that each leave a marker.
fn hooked(n: u32) -> Fleet {
    Fleet::with_phases("", ONE_PHASE)
        .module(
            "app",
            "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\nhealth:\n  - name: healthy\n    interval: 1s\n    file: { path: /run/app.healthy }\nactions:\n  '@suspend':\n    file.copy: { src: marker, dest: /run/app.suspended, mode: \"0644\" }\n  '@resume':\n    file.copy: { src: marker, dest: /run/app.resumed, mode: \"0644\" }\n",
        )
        .file("app", "app.conf", common::app_conf(n))
        .file("app", "marker", "done\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_module_sits_out_a_rollout_and_catches_up_when_resumed() {
    let mut world = World::start(hooked(1)).await;
    let v1 = world.source_revision();
    let host = world.spawn(common::host("sus-1").drift(Duration::from_secs(1), true));
    world.converge(&v1, &["sus-1"]).await;

    let unit = |note: &str| serde_json::json!({"target": "sus-1", "unit": "app", "note": note});
    world
        .api()
        .post_ok("/api/v1/units/suspend", unit("disk swap"))
        .await;
    eventually("the @suspend hook to run", WAIT, async || {
        host.file(SUSPENDED).ok_or("not yet".to_owned())
    })
    .await;
    assert_eq!(host.agent_state().paused_modules, vec!["app"]);
    let paused: Vec<PausedUnit> = world.api().get("/api/v1/units/paused").await;
    assert_eq!(paused.len(), 1, "{paused:?}");

    // The next revision reaches the host, and passes the module by.
    let v2 = world.publish(&hooked(2)).await.id;
    world.converge(&v2, &["sus-1"]).await;
    assert_eq!(host.file(APP_CONF), Some(common::app_conf(1)), "left alone");
    eventually("its checks to read paused", WAIT, async || {
        let results: Vec<HealthResultDto> = world
            .api()
            .try_get("/api/v1/hosts/sus-1/health")
            .await
            .ok_or("unavailable")?;
        results
            .first()
            .filter(|r| r.module == "app" && r.status == "paused")
            .map(|_| ())
            .ok_or(format!("{results:?}"))
    })
    .await;

    world
        .api()
        .post_ok("/api/v1/units/resume", unit("disk swapped"))
        .await;
    eventually("the resumed module to catch up", WAIT, async || {
        (host.file(APP_CONF) == Some(common::app_conf(2)))
            .then_some(())
            .ok_or(format!("{:?}", host.file(APP_CONF)))
    })
    .await;
    assert!(host.file(RESUMED).is_some(), "its @resume ran first");
    let paused: Vec<PausedUnit> = world.api().get("/api/v1/units/paused").await;
    assert!(paused.is_empty(), "{paused:?}");
    eventually("the agent to forget the catch-up", WAIT, async || {
        let state = host.agent_state();
        (state.paused_modules.is_empty() && state.resumed_modules.is_empty())
            .then_some(())
            .ok_or(format!("{:?}", state.resumed_modules))
    })
    .await;
    world.shutdown().await;
}
