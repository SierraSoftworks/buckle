//! The `docker` fact: engine version and whether the daemon is up.

use super::probe;
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Describes the local Docker engine. `null` unless a `docker` binary is on the PATH.
///
/// Keys: `version` (the server version from `docker version --format
/// '{{.Server.Version}}'`; `null` when the daemon does not answer) and
/// `running` (whether the daemon answered). Refreshed every minute.
pub struct DockerFacts;

#[async_trait]
impl FactProvider for DockerFacts {
    const NAME: &'static str = "docker";
    const TTL: Duration = Duration::from_secs(60);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Docker engine version and whether the daemon is running (null when not installed).";

    #[tracing::instrument(name = "facts.docker", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        if cx.system.which("docker").await?.is_none() {
            return Ok(Value::Null);
        }
        let version = probe(
            cx,
            "docker",
            &["version", "--format", "{{.Server.Version}}"],
        )
        .await
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
        Ok(json!({"running": version.is_some(), "version": version}))
    }
}

buckle_core::register_facts!(DockerFacts);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::testing::FakeSystem;

    #[tokio::test]
    async fn running_daemon() {
        let sys = FakeSystem::new().on_command_ok("docker", &["version"], "26.1.4\n");
        let f = DockerFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f, json!({"running": true, "version": "26.1.4"}));
        assert_eq!(
            sys.commands_run()[0].args,
            vec!["version", "--format", "{{.Server.Version}}"]
        );
    }

    #[tokio::test]
    async fn stopped_daemon_and_absent_binary() {
        let sys = FakeSystem::new().on_command_fail(
            "docker",
            &["version"],
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        );
        let f = DockerFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f, json!({"running": false, "version": null}));
        assert!(
            DockerFacts
                .collect(&cx(&FakeSystem::new()))
                .await
                .unwrap()
                .is_null()
        );
    }
}
