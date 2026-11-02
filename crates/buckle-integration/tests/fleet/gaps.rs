//! Design behaviours the implementation does not meet yet. Each test asserts
//! what the design says and is ignored with the reason it fails today, so
//! `cargo test -p buckle-integration -- --ignored` shows what is left and the
//! tests start passing as the gaps close.

use crate::common::{self, APP_CONF, ONE_PHASE};
use buckle_api::{HostRolloutState, RolloutState};
use buckle_core::system::Output;
use buckle_core::testing::ok_output;
use buckle_integration::{Fleet, WAIT, World, describe, eventually, rollout_host};
use std::time::Duration;

/// Design §08 "Registration": an approved host is active and §08 "Identity":
/// server labels win. A connected agent only learns its status and labels from
/// Welcome, and the server sends Welcome once per session, so an agent stays
/// `pending` (and on stale labels) until it reconnects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gap: approval and label changes are not pushed to a connected agent (grpc/service.rs sends Welcome only at session start)"]
async fn an_approved_host_learns_its_status_and_labels_without_reconnecting() {
    let mut world = World::start_with(
        common::app(ONE_PHASE, 1),
        buckle_integration::server::ServerSettings {
            registration: "approve",
            ..Default::default()
        },
    )
    .await;
    let v1 = world.source_revision();
    let agent = world.spawn(common::host("new-1"));
    world
        .wait_host("new-1", "pending", |h| {
            h.status == buckle_api::HostStatus::Pending
        })
        .await;
    world
        .api()
        .patch_ok(
            "/api/v1/hosts/new-1",
            serde_json::json!({ "labels": { "role": "web" } }),
        )
        .await;
    world
        .api()
        .post_ok("/api/v1/hosts/new-1/approve", serde_json::json!({}))
        .await;
    world.converge(&v1, &["new-1"]).await;
    eventually("the agent to learn it is active", WAIT, async || {
        let state = agent.agent_state();
        (state.status.as_deref() == Some("active")
            && state.labels.get("role").map(String::as_str) == Some("web"))
        .then_some(())
        .ok_or(format!("{:?} {:?}", state.status, state.labels))
    })
    .await;
    world.shutdown().await;
}

/// Design §08 "Registration": a paused host is excluded from drift
/// correction. The agent decides from the status in its last Welcome, which a
/// pause does not refresh, so it keeps correcting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gap: pausing a host does not reach its connected agent, which keeps correcting drift (session.rs drift() reads the Welcome status)"]
async fn a_paused_host_stops_correcting_drift() {
    let mut world = World::start(common::app(ONE_PHASE, 1)).await;
    let v1 = world.source_revision();
    let host = world.spawn(common::host("drift-1").drift(Duration::from_secs(1), true));
    world.converge(&v1, &["drift-1"]).await;
    world
        .api()
        .post_ok("/api/v1/hosts/drift-1/pause", serde_json::json!({}))
        .await;
    host.write_file(APP_CONF, "hand edit\n");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        host.file(APP_CONF),
        Some("hand edit\n".to_owned()),
        "a paused host corrected drift"
    );
    world.shutdown().await;
}

/// Design §10 "Waves": failures over `max_failures` pause the phase and
/// "nothing else is started"; the operator moves past a failed host with
/// retry, continue, skip or a resume that accepts it. When an operator acts on
/// a different host, the controller unpauses, and once that host finishes the
/// phase counts the still-failed host as finished, passes its gate and
/// promotes the revision with the breach unresolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "design violation: a phase over its failure budget completes and promotes once every other host finishes (rollout/controller.rs drive_phases `done` treats Failed as finished)"]
async fn a_phase_over_its_failure_budget_does_not_complete_while_the_failure_stands() {
    let fleet = |n: u32| {
        let deploy = if n > 1 {
            "  - command.run: { run: [deploy-app], creates: /etc/app/deployed }\n"
        } else {
            ""
        };
        Fleet::with_phases("", "    - { name: fleet, select: 'true', parallel: 10, approval: auto, bake: 1s, max_failures: 0 }\n")
            .module("app", format!("name: app\ntasks:\n  - file.copy: {{ src: app.conf, dest: /etc/app/app.conf }}\n{deploy}"))
            .file("app", "app.conf", common::app_conf(n))
    };
    let mut world = World::start(fleet(1)).await;
    let v1 = world.source_revision();
    // Both fail to deploy revision 2: one is fixed and retried, the other
    // is left failed.
    let fail = |id: &str| {
        common::healthy_system(id).on_command("deploy-app", &[], |_| Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"refused\n".to_vec(),
            timed_out: false,
        })
    };
    // Revision 1 has no deploy step, so both hosts converge on it first.
    let a = world.spawn(common::host("a-1").system(fail("a-1")));
    world.spawn(common::host("b-1").system(fail("b-1")));
    world.converge(&v1, &["a-1", "b-1"]).await;
    let v2 = world.publish(&fleet(2)).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    world
        .wait_rollout(&id, "both failed and paused", |d| {
            d.summary.state == RolloutState::Paused
                && ["a-1", "b-1"].iter().all(|h| {
                    rollout_host(d, h).is_some_and(|r| r.state == HostRolloutState::Failed)
                })
        })
        .await;
    let _ = a
        .system
        .clone()
        .on_command("deploy-app", &[], |_| ok_output(""));
    world
        .api()
        .post_ok(
            &format!("/api/v1/rollouts/{id}/hosts/a-1/retry"),
            serde_json::json!({}),
        )
        .await;
    world
        .wait_rollout_host(&id, "a-1", HostRolloutState::Healthy)
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let detail: buckle_api::RolloutDetail =
        world.api().get(&format!("/api/v1/rollouts/{id}")).await;
    assert_ne!(
        detail.summary.state,
        RolloutState::Completed,
        "promoted with b-1 failed: {}",
        describe(&detail)
    );
    world.shutdown().await;
}

/// Design §06 "What the operator sees": a rotated secret shows up as a
/// fingerprint change on exactly the lines that use it (the plan diffs the
/// recorded redacted shadow against the new redacted render). file.template
/// never consults the recorded shadow for a sensitive render, so the plan
/// says "content changed (digest a → b)" with only the new render.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gap: a secret rotation is shown as an opaque digest change, not a shadow diff of the fingerprint (buckle-std tasks/file_template.rs inspect ignores cx.previous)"]
async fn a_rotated_secret_is_a_fingerprint_change_on_the_line_that_uses_it() {
    let fleet = |template: &str| {
        let base = Fleet::with_phases("", ONE_PHASE);
        let yaml = base.yaml().replacen(
            "rollout:\n",
            "secrets:\n  token: { file: token }\nrollout:\n",
            1,
        );
        base.buckle_yaml(yaml)
            .module(
                "svc",
                "name: svc\ntasks:\n  - file.template: { src: svc.j2, dest: /etc/svc.conf }\n",
            )
            .file("svc", "svc.j2", template)
    };
    let mut world = World::start(fleet("token = {{ secrets.token }}\nmode = a\n")).await;
    world.write_secret("token", "first-value");
    let v1 = world.source_revision();
    world.spawn(common::host("svc-1"));
    world.converge(&v1, &["svc-1"]).await;
    world.write_secret("token", "second-value");
    // Approval required so the plan can be read before it applies.
    let reviewed = fleet("token = {{ secrets.token }}\nmode = a\n").buckle_yaml(
        fleet("")
            .yaml()
            .replace("approval: auto", "approval: required"),
    );
    let v2 = world.publish(&reviewed).await.id;
    let id = world.rollout_for(&v2).await.summary.id;
    let detail = world
        .wait_rollout(&id, "planned", |d| {
            rollout_host(d, "svc-1").is_some_and(|h| h.needs_approval)
        })
        .await;
    let plan = serde_json::to_string(&detail.plans).unwrap();
    assert!(
        plan.contains("-token = «secret:token@") && plan.contains("+token = «secret:token@"),
        "{plan}"
    );
    world.shutdown().await;
}

/// Design §03: label values are typed like YAML (`canary: "true"` is the
/// boolean). The server coerces label strings when it evaluates groups,
/// phases and secret scoping; the agent hands its labels to the engine as
/// strings, so a module selector comparing a label to a boolean must select
/// the same hosts on both sides. It does not: the server puts the host in the
/// `canary` group, the agent's engine does not select the module.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "design violation: the agent passes labels to the engine as strings while the server coerces them (buckle-agent session.rs labels_json vs buckle-server fleet.rs labels_to_core)"]
async fn a_boolean_label_selects_the_same_modules_on_the_agent_as_on_the_server() {
    let fleet = common::app(ONE_PHASE, 1)
        .module(
            "canary_only",
            "name: canary_only\nwhen: 'host.labels.canary == true'\ntasks:\n  - file.copy: { src: flag, dest: /etc/canary }\n",
        )
        .file("canary_only", "flag", "yes\n");
    let mut world = World::start(fleet).await;
    let v1 = world.source_revision();
    let canary = world.spawn(common::host("canary-1").label("canary", "true"));
    world.converge(&v1, &["canary-1"]).await;
    let preview: buckle_api::HostPreview = serde_json::from_str(
        &world
            .api()
            .post_ok("/api/v1/hosts/canary-1/preview", serde_json::json!({}))
            .await,
    )
    .unwrap();
    assert_eq!(
        preview.groups,
        vec!["canary".to_owned()],
        "the server sees a boolean"
    );
    assert_eq!(canary.file("/etc/canary"), Some("yes\n".to_owned()));
    world.shutdown().await;
}
