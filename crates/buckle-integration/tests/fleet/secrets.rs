//! Scenario 6: secrets cannot leak by accident (design §06). The server
//! resolves a file secret and delivers it to the hosts whose modules use it;
//! a template renders it through the file sink. The value reaches the file
//! on the host and nowhere else: not the API (plans, diffs, rollout events,
//! audit, host views), not the event stream, not a log line of the server,
//! agents or engine, not any report an agent sent, not the server's data
//! directory. The reference token `«secret:NAME@fingerprint»` is what shows.

use crate::common::{self, ONE_PHASE};
use buckle_agent::transport::convert::report_to_proto;
use buckle_api::{HostSummary, Page, RolloutSummary};
use buckle_integration::{Fleet, World, logs};
use prost::Message;
use std::collections::BTreeSet;
use std::path::Path;

const TOKEN: &str = "«secret:db_password@";

fn fleet(template: &str) -> Fleet {
    let base = Fleet::with_phases("", ONE_PHASE);
    let yaml = base.yaml().replacen(
        "rollout:\n",
        "secrets:\n  db_password: { file: db_password }\nrollout:\n",
        1,
    );
    common::app_module(base.buckle_yaml(yaml), 1)
        .module(
            "db",
            "name: db\ntasks:\n  - file.template: { src: credentials.j2, dest: /etc/db/credentials, mode: \"0600\" }\n",
        )
        .file("db", "credentials.j2", template)
}

/// The fingerprints that follow `«secret:db_password@` in `text`.
fn fingerprints(text: &str) -> BTreeSet<String> {
    text.match_indices(TOKEN)
        .map(|(i, _)| {
            text[i + TOKEN.len()..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect::<String>()
        })
        .filter(|f| !f.is_empty())
        .collect()
}

fn files_containing(dir: &Path, needle: &[u8], out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_containing(&path, needle, out);
        } else if let Ok(bytes) = std::fs::read(&path)
            && bytes.windows(needle.len()).any(|w| w == needle)
        {
            out.push(path.display().to_string());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_values_reach_the_sink_and_nothing_else_while_the_reference_token_does() {
    let first = format!("first-{}", uuid::Uuid::new_v4().simple());
    let second = format!("second-{}", uuid::Uuid::new_v4().simple());
    let template_v1 = "user = app\npassword = {{ secrets.db_password }}\n";
    let template_v2 = "user = app\npassword = {{ secrets.db_password }}\npool = 10\n";

    let mut world = World::start(fleet(template_v1)).await;
    world.write_secret("db_password", &first);
    let v1 = world.source_revision();
    let sse = world.sse().await;
    let hosts = ["db-1", "db-2"];
    for h in hosts {
        world.spawn(common::host(h));
    }
    world.converge(&v1, &hosts).await;
    for h in hosts {
        assert_eq!(
            world.agent(h).file("/etc/db/credentials"),
            Some(format!("user = app\npassword = {first}\n")),
            "{h}: the sink wrote the real value"
        );
    }

    // Rotate the secret alongside a template change.
    world.write_secret("db_password", &second);
    let v2 = world.publish(&fleet(template_v2)).await.id;
    world.converge(&v2, &hosts).await;
    for h in hosts {
        assert_eq!(
            world.agent(h).file("/etc/db/credentials"),
            Some(format!("user = app\npassword = {second}\npool = 10\n")),
            "{h}: the rotated value reached the file"
        );
    }

    // Read everything an operator can: rollouts and their plans, host
    // views, runs, health, resources, revisions, the audit log.
    let api = world.api();
    let rollouts: Page<RolloutSummary> = api.get("/api/v1/rollouts").await;
    for r in &rollouts.items {
        let _: serde_json::Value = api.get(&format!("/api/v1/rollouts/{}", r.id)).await;
        let _: serde_json::Value = api.get(&format!("/api/v1/rollouts/{}/plans", r.id)).await;
    }
    for h in hosts {
        for path in ["", "/facts", "/state", "/health", "/runs"] {
            let _: serde_json::Value = api.get(&format!("/api/v1/hosts/{h}{path}")).await;
        }
    }
    let _: Vec<HostSummary> = api.get("/api/v1/hosts").await;
    let _: serde_json::Value = api.get("/api/v1/fleet").await;
    let _: serde_json::Value = api.get("/api/v1/revisions").await;
    let _: serde_json::Value = api.get("/api/v1/audit?limit=1000").await;

    let journal = world.journal();
    let stream = sse.raw();
    let reports: Vec<(String, Vec<u8>)> = hosts
        .iter()
        .flat_map(|h| world.agent(h).reports())
        .map(|r| (format!("{r:?}"), report_to_proto(&r).encode_to_vec()))
        .collect();
    assert!(!reports.is_empty());
    for secret in [&first, &second] {
        assert!(
            !journal.contains(secret.as_str()),
            "the API returned a secret value"
        );
        assert!(
            !stream.contains(secret.as_str()),
            "the event stream carried a secret value"
        );
        assert!(
            !logs::contains(secret),
            "a log line carried a secret value: {:?}",
            logs::lines_with(secret)
        );
        for (debug, wire) in &reports {
            assert!(!debug.contains(secret.as_str()), "an agent report: {debug}");
            assert!(
                !wire.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "an agent report's wire form carried a secret value"
            );
        }
        let mut stored = Vec::new();
        files_containing(&world.paths.data, secret.as_bytes(), &mut stored);
        assert!(
            stored.is_empty(),
            "the server stored a secret value in {stored:?}"
        );
    }

    // What operators see instead: the reference token, whose fingerprint
    // changed with the rotation, in the plans and in what the agents sent.
    let shown = fingerprints(&journal);
    assert!(
        shown.len() >= 2,
        "plans show a token per value, got {shown:?}"
    );
    assert!(
        reports.iter().any(|(debug, _)| debug.contains(TOKEN)),
        "agent plans carry the token"
    );
    assert!(!stream.is_empty());
    sse.close();
    world.shutdown().await;
}
