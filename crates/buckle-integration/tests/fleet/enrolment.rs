//! Scenario 1: enrolment and registration (design §08 "Identity",
//! "Registration").

use crate::common::{self, APP_CONF};
use buckle_api::{Connection, HostStatus};
use buckle_integration::server::ServerSettings;
use buckle_integration::{AgentSpec, WAIT, World, eventually, rollout_host};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_host_waits_for_approval_then_converges_and_is_welcomed_with_its_labels() {
    let mut world = World::start_with(
        common::app(common::ONE_PHASE, 1),
        ServerSettings {
            registration: "approve",
            ..Default::default()
        },
    )
    .await;
    let v1 = world.source_revision();

    // First contact with the bootstrap token: the server creates the host as
    // pending and issues a per-host token, which the agent stores.
    let agent = world.spawn(common::host("new-1").label("site", "home"));
    let host = world
        .wait_host("new-1", "registered, pending and online", |h| {
            h.status == HostStatus::Pending && h.connection == Connection::Online
        })
        .await;
    assert_eq!(
        host.agent_labels.get("site").map(String::as_str),
        Some("home")
    );
    eventually("the per-host token to be stored", WAIT, async || {
        agent.token().map(|_| ()).ok_or("no token".to_owned())
    })
    .await;
    assert_eq!(agent.token().map(|t| t.len()), Some(64));
    eventually("Welcome to report the pending status", WAIT, async || {
        let state = agent.agent_state();
        (state.status.as_deref() == Some("pending"))
            .then_some(())
            .ok_or(format!("{:?}", state.status))
    })
    .await;

    // Pending hosts are excluded from rollouts: the first rollout's open
    // stragglers stage does not pick it up, and nothing is applied.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let first = world.rollout_for(&v1).await;
    assert!(
        rollout_host(&first, "new-1").is_none(),
        "a pending host joined a rollout: {}",
        buckle_integration::describe(&first)
    );
    assert_eq!(agent.file(APP_CONF), None, "nothing was applied");

    // An operator sets a server-side label and approves the host.
    world
        .api()
        .patch_ok(
            "/api/v1/hosts/new-1",
            serde_json::json!({ "labels": { "role": "web", "site": "lab" } }),
        )
        .await;
    world
        .api()
        .post_ok("/api/v1/hosts/new-1/approve", serde_json::json!({}))
        .await;
    let host = world
        .wait_host("new-1", "active", |h| h.status == HostStatus::Active)
        .await;
    assert_eq!(
        host.effective_labels.get("site").map(String::as_str),
        Some("lab"),
        "server labels win on conflict"
    );

    // Approved, it joins the stragglers stage and converges on the stable revision.
    let first = world.converge(&v1, &["new-1"]).await;
    assert_eq!(
        rollout_host(&first, "new-1").map(|h| h.phase.as_str()),
        Some("stragglers")
    );
    assert_eq!(agent.file(APP_CONF), Some(common::app_conf(1)));

    // On its next session the agent authenticates with its per-host token and
    // Welcome carries the active status and the effective labels.
    world
        .stop_agent("new-1")
        .await
        .expect("the agent stops cleanly");
    let agent = world.restart_agent("new-1");
    eventually(
        "Welcome to mark the agent active with the server's labels",
        WAIT,
        async || {
            let state = agent.agent_state();
            let ok = state.status.as_deref() == Some("active")
                && state.labels.get("role").map(String::as_str) == Some("web")
                && state.labels.get("site").map(String::as_str) == Some("lab");
            ok.then_some(())
                .ok_or(format!("{:?} {:?}", state.status, state.labels))
        },
    )
    .await;

    // A second machine claiming the enrolled id with the bootstrap token is
    // refused, and so is a wrong bootstrap token; neither agent retries.
    world.spawn(common::host("new-1").state_dir("impostor"));
    world.spawn(AgentSpec::new("intruder").bootstrap_token("not-the-token"));
    for name in ["impostor", "intruder"] {
        eventually(&format!("the {name} agent to give up"), WAIT, async || {
            world
                .agent_exited(name)
                .then_some(())
                .ok_or("still running".to_owned())
        })
        .await;
        let err = world
            .stop_agent(name)
            .await
            .expect_err("a rejected agent exits with an error");
        assert!(
            err.description().contains("rejected"),
            "{name}: {}",
            err.description()
        );
    }
    let (status, _) = world
        .api()
        .request(reqwest::Method::GET, "/api/v1/hosts/intruder", None)
        .await
        .expect("the API answers");
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        world
            .api()
            .get::<Vec<buckle_api::HostSummary>>("/api/v1/hosts")
            .await
            .len(),
        1,
        "only the enrolled host exists"
    );
    world.shutdown().await;
}
