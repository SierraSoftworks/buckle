//! The `tcp` health check.

use async_trait::async_trait;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::task::Cx;
use humantime_serde::re::humantime::format_duration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Opens a TCP connection to `address` (`host:port`) and passes when it is accepted.
///
/// A refused connection or one that does not complete within `timeout`
/// (default 5s) fails. Use it to check that a service is listening.
pub struct TcpCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The address to connect to, e.g. `127.0.0.1:4647`.
    pub address: String,
    /// How long to wait for the connection; defaults to 5s.
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

fn default_timeout() -> Duration {
    Duration::from_secs(5)
}

#[async_trait]
impl HealthCheck for TcpCheck {
    type Spec = Spec;
    const KIND: &'static str = "tcp";
    const DESCRIPTION: &'static str = "Check that a TCP port accepts connections.";

    #[tracing::instrument(name = "health.tcp", skip_all, fields(address = %spec.address))]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        match cx.system.tcp_connect(&spec.address, spec.timeout).await {
            Ok(true) => HealthStatus::Pass,
            Ok(false) => HealthStatus::fail(format!(
                "nothing accepted a connection on {} within {}",
                spec.address,
                format_duration(spec.timeout)
            )),
            Err(e) => HealthStatus::fail(format!(
                "connecting to {} failed: {}",
                spec.address,
                e.description()
            )),
        }
    }
}

buckle_core::register_health!(TcpCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::listening("127.0.0.1:4647", true)]
    #[case::closed("127.0.0.1:4648", false)]
    #[case::other_host("10.0.0.5:4647", false)]
    #[tokio::test]
    async fn passes_only_for_open_ports(#[case] address: &str, #[case] open: bool) {
        let sys = FakeSystem::new().with_tcp_open("127.0.0.1:4647");
        let result = TcpCheck
            .check(&cx(&sys), &spec(json!({"address": address})))
            .await;
        if open {
            assert_eq!(result, HealthStatus::Pass);
        } else {
            match result {
                HealthStatus::Fail { reason } => {
                    assert!(
                        reason.contains(address) && reason.contains("5s"),
                        "{reason}"
                    )
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn timeout_is_configurable() {
        let s: Spec = spec(json!({"address": "a:1", "timeout": "250ms"}));
        assert_eq!(s.timeout, Duration::from_millis(250));
    }
}
