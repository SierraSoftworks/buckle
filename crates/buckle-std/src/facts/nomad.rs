//! The `nomad` fact: version and scheduling eligibility of the local Nomad agent.

use super::{probe, probe_json};
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

/// Describes the local Nomad installation. `null` unless a `nomad` binary is on the PATH.
///
/// Keys: `version` (`nomad version`, without the leading `v`), `eligibility`
/// (`eligible` or `ineligible`, from `nomad node status -self -json`), `drain`
/// (whether the node is draining) and `status` (the node's status, e.g.
/// `ready`). The node keys are `null` when the agent is not running as a
/// client or cannot be reached. Refreshed every minute.
pub struct NomadFacts;

static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Nomad\s+v?(\S+)").expect("valid regex"));

pub(crate) fn parse_version(text: &str) -> Option<String> {
    VERSION.captures(text).map(|c| c[1].to_owned())
}

#[async_trait]
impl FactProvider for NomadFacts {
    const NAME: &'static str = "nomad";
    const TTL: Duration = Duration::from_secs(60);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Nomad version and node eligibility (null when nomad is not installed).";

    #[tracing::instrument(name = "facts.nomad", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        if cx.system.which("nomad").await?.is_none() {
            return Ok(Value::Null);
        }
        let version = probe(cx, "nomad", &["version"])
            .await
            .and_then(|v| parse_version(&v));
        let node = probe_json(cx, "nomad", &["node", "status", "-self", "-json"]).await;
        let node = node.as_ref();
        let field = |name: &str| {
            node.and_then(|n| n.get(name))
                .cloned()
                .unwrap_or(Value::Null)
        };
        Ok(json!({
            "version": version,
            "eligibility": field("SchedulingEligibility"),
            "drain": field("Drain"),
            "status": field("Status"),
        }))
    }
}

buckle_core::register_facts!(NomadFacts);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::testing::FakeSystem;

    const VERSION_OUT: &str = "Nomad v1.7.5\nBuildDate 2024-03-12T09:43:11Z\nRevision 5b4dc0e9f5adc1c9a1a2e0b2e8b9d0a2e8e5e6a1+CHANGES\n";
    const NODE: &str = r#"{"ID":"7c4cc8d1-1f8c-d9a0-4b1d-0a7e8d1b9e25","Datacenter":"london","Name":"web-1","NodeClass":"","Drain":false,"SchedulingEligibility":"ineligible","Status":"ready","StatusDescription":""}"#;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version(VERSION_OUT).as_deref(), Some("1.7.5"));
        assert_eq!(parse_version("Nomad 1.6.0\n").as_deref(), Some("1.6.0"));
        assert_eq!(parse_version("nope"), None);
    }

    #[tokio::test]
    async fn reports_version_and_eligibility() {
        let sys = FakeSystem::new()
            .on_command_ok("nomad", &["version"], VERSION_OUT)
            .on_command_ok("nomad", &["node", "status"], NODE);
        let f = NomadFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["version"], "1.7.5");
        assert_eq!(f["eligibility"], "ineligible");
        assert_eq!(f["drain"], false);
        assert_eq!(f["status"], "ready");
    }

    #[tokio::test]
    async fn node_keys_are_null_when_the_agent_is_not_reachable() {
        let sys = FakeSystem::new()
            .on_command_ok("nomad", &["version"], VERSION_OUT)
            .on_command_fail(
                "nomad",
                &["node", "status"],
                1,
                "Error querying node info: connection refused",
            );
        let f = NomadFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["version"], "1.7.5");
        assert!(f["eligibility"].is_null() && f["drain"].is_null());
    }

    #[tokio::test]
    async fn absent_without_the_binary() {
        assert!(
            NomadFacts
                .collect(&cx(&FakeSystem::new()))
                .await
                .unwrap()
                .is_null()
        );
    }
}
