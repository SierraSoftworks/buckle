//! The `tailscale` fact: this node's identity on the tailnet.

use super::probe_json;
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Describes this node's Tailscale state from `tailscale status --json`.
///
/// Keys: `hostname` (the node's MagicDNS name without the trailing dot),
/// `ips` (its tailnet addresses), `tags` (ACL tags such as `tag:server`;
/// empty for user-owned nodes), `online`, `backend_state` (`Running`,
/// `Stopped`, `NeedsLogin`, ...) and `tailnet` (the tailnet name). The whole
/// fact is `null` when `tailscale` is not installed or does not answer.
/// Refreshed every five minutes.
pub struct TailscaleFacts;

pub(crate) fn parse_status(status: &Value) -> Value {
    let me = &status["Self"];
    if !me.is_object() {
        return Value::Null;
    }
    let strings = |v: &Value| -> Vec<Value> {
        v.as_array()
            .map(|a| a.iter().filter(|s| s.is_string()).cloned().collect())
            .unwrap_or_default()
    };
    json!({
        "hostname": me["DNSName"].as_str().map(|d| d.trim_end_matches('.')),
        "ips": strings(&me["TailscaleIPs"]),
        "tags": strings(&me["Tags"]),
        "online": me["Online"].as_bool().unwrap_or(false),
        "backend_state": status["BackendState"],
        "tailnet": status["CurrentTailnet"]["Name"],
    })
}

#[async_trait]
impl FactProvider for TailscaleFacts {
    const NAME: &'static str = "tailscale";
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Tailscale identity: hostname, ips, tags, online (null when not installed).";

    #[tracing::instrument(name = "facts.tailscale", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        if cx.system.which("tailscale").await?.is_none() {
            return Ok(Value::Null);
        }
        Ok(probe_json(cx, "tailscale", &["status", "--json"])
            .await
            .map(|s| parse_status(&s))
            .unwrap_or(Value::Null))
    }
}

buckle_core::register_facts!(TailscaleFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::testing::FakeSystem;

    pub const STATUS: &str = r#"{
  "Version": "1.74.1-t5d6c7e8f9-g1a2b3c4d5",
  "TUN": true,
  "BackendState": "Running",
  "HaveNodeKey": true,
  "AuthURL": "",
  "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1234"],
  "Self": {
    "ID": "n1234CNTRL",
    "PublicKey": "nodekey:abcdef",
    "HostName": "web-1",
    "DNSName": "web-1.tail1234.ts.net.",
    "OS": "linux",
    "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1234"],
    "Tags": ["tag:server", "tag:web"],
    "Online": true,
    "InNetworkMap": true,
    "InMagicSock": true
  },
  "Health": [],
  "MagicDNSSuffix": "tail1234.ts.net",
  "CurrentTailnet": {"Name": "example.com", "MagicDNSSuffix": "tail1234.ts.net", "MagicDNSEnabled": true},
  "Peer": {}
}"#;

    #[tokio::test]
    async fn parses_the_status_document() {
        let sys = FakeSystem::new().with_program("tailscale").on_command_ok(
            "tailscale",
            &["status", "--json"],
            STATUS,
        );
        let f = TailscaleFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["hostname"], "web-1.tail1234.ts.net");
        assert_eq!(f["ips"], json!(["100.101.102.103", "fd7a:115c:a1e0::1234"]));
        assert_eq!(f["tags"], json!(["tag:server", "tag:web"]));
        assert_eq!(f["online"], true);
        assert_eq!(f["backend_state"], "Running");
        assert_eq!(f["tailnet"], "example.com");
    }

    #[tokio::test]
    async fn user_owned_nodes_have_no_tags_and_stopped_nodes_are_offline() {
        let stopped = r#"{"BackendState":"Stopped","Self":{"DNSName":"laptop.tail1234.ts.net.","TailscaleIPs":["100.64.0.9"]}}"#;
        let sys = FakeSystem::new().with_program("tailscale").on_command_ok(
            "tailscale",
            &["status"],
            stopped,
        );
        let f = TailscaleFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["tags"], json!([]));
        assert_eq!(f["online"], false);
        assert_eq!(f["backend_state"], "Stopped");
        assert!(f["tailnet"].is_null());
    }

    #[tokio::test]
    async fn null_when_absent_or_broken() {
        // Not installed.
        let f = TailscaleFacts
            .collect(&cx(&FakeSystem::new()))
            .await
            .unwrap();
        assert!(f.is_null());
        // Installed but not answering with JSON.
        let sys = FakeSystem::new().with_program("tailscale").on_command_ok(
            "tailscale",
            &["status"],
            "Tailscale is stopped.\n",
        );
        assert!(TailscaleFacts.collect(&cx(&sys)).await.unwrap().is_null());
        // Daemon not running.
        let sys = FakeSystem::new().with_program("tailscale").on_command_fail(
            "tailscale",
            &["status"],
            1,
            "failed to connect to local tailscaled",
        );
        assert!(TailscaleFacts.collect(&cx(&sys)).await.unwrap().is_null());
    }
}
