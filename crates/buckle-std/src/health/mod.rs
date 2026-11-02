//! Health check kinds. See the design document section 07.
//!
//! A check answers one question about the host and returns
//! [`buckle_core::health::HealthStatus`]: `Pass`, `Fail { reason }` when the
//! thing it looks at is wrong (including a service that is down), or
//! `Unknown { reason }` when the check itself could not run (a missing tool,
//! an unsupported platform, an invalid expression). Whether a failure stops a
//! rollout (`required`), how often a check repeats (`interval`) and its
//! `timeout` belong to the declaration in `module.yaml`, not to the check.

pub mod command;
pub mod file;
pub mod filter;
pub mod http;
pub mod systemd;
pub mod tcp;

/// Runs a check's reason through the secret scrubber, so a failing command's
/// output or an HTTP body that echoes a secret never reaches the UI or logs.
pub(crate) fn redacted(
    cx: &buckle_core::task::Cx,
    status: buckle_core::health::HealthStatus,
) -> buckle_core::health::HealthStatus {
    use buckle_core::health::HealthStatus;
    let scrub = |reason: String| {
        if cx.secrets.is_empty() {
            reason
        } else {
            buckle_core::secrets::redact::Redactor::new(&cx.secrets)
                .redact(&reason)
                .into_inner()
        }
    };
    match status {
        HealthStatus::Fail { reason } => HealthStatus::Fail {
            reason: scrub(reason),
        },
        HealthStatus::Unknown { reason } => HealthStatus::Unknown {
            reason: scrub(reason),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::health::{HealthStatus, lookup, registry};
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::FakeSystem;
    use serde_json::json;

    #[test]
    fn every_documented_kind_is_registered() {
        for kind in ["http", "tcp", "systemd", "command", "filter", "file"] {
            assert!(registry().contains_key(kind), "{kind} is not registered");
            assert!(
                !lookup(kind).unwrap().description().is_empty(),
                "{kind} has no description"
            );
        }
    }

    #[test]
    fn schemas_describe_the_fields() {
        let schema = serde_json::to_value(lookup("http").unwrap().schema()).unwrap();
        assert!(schema["properties"]["url"].is_object());
        assert!(schema["properties"]["expect"].is_object());
    }

    #[tokio::test]
    async fn checks_run_through_the_erased_registry_like_the_engine_does() {
        let sys = FakeSystem::new()
            .on_http("http://127.0.0.1:4646/", |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: b"{\"ok\":true}".to_vec(),
            })
            .with_tcp_open("127.0.0.1:4647");
        let cx = cx(&sys);
        let http = lookup("http").unwrap();
        let spec = http
            .parse(json!({"url": "http://127.0.0.1:4646/v1/agent/health"}))
            .unwrap();
        assert_eq!(http.check(&cx, &spec).await, HealthStatus::Pass);
        let tcp = lookup("tcp").unwrap();
        let spec = tcp.parse(json!({"address": "127.0.0.1:4648"})).unwrap();
        assert!(matches!(
            tcp.check(&cx, &spec).await,
            HealthStatus::Fail { .. }
        ));
        assert!(
            http.parse(json!({"nope": 1})).is_err(),
            "unknown fields are rejected"
        );
    }
}
