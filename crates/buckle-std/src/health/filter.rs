//! The `filter` health check.

use async_trait::async_trait;
use buckle_core::Filter;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::task::Cx;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Evaluates a filt-rs expression against the host's facts, labels and vars.
///
/// The expression sees the same context as a module `when:`: `host.id`,
/// `host.labels.*`, `host.groups`, `host.facts.*` and `vars.*`. It passes when
/// the expression is true and fails when it is false. Use it for conditions
/// such as `host.facts.hardware.disk.root.free_percent > 10`; facts cannot be
/// indexed, so providers expose flat keys for exactly this. A fact that is
/// missing (its provider returned `null`, or failed without ever producing a
/// value) makes comparisons false, so the check fails; guard with
/// `host.facts.x != null && ...` if absence should pass. A provider that fails
/// after an earlier success keeps its last good value.
///
/// The check reads the facts already in the context, it does not collect
/// them. After an apply, a module's checks see the facts refreshed by the
/// providers it declares under `facts.provides` (re-run before its checks)
/// and by modules applied before it. Outside an apply (`buckle health` and
/// the baseline, rollout and drift scopes) the engine collects facts by
/// provider TTL first, so a value can be up to one TTL old. An expression that
/// does not parse is unknown, not failed.
pub struct FilterCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The filt-rs expression, e.g. `host.facts.hardware.disk.root.free_percent > 10`.
    pub expr: String,
}

#[async_trait]
impl HealthCheck for FilterCheck {
    type Spec = Spec;
    const KIND: &'static str = "filter";
    const DESCRIPTION: &'static str =
        "Evaluate a filter expression over the host's facts (for example free disk space).";

    #[tracing::instrument(name = "health.filter", skip_all, fields(expr = %spec.expr))]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        let filter = match Filter::new(spec.expr.as_str()) {
            Ok(f) => f,
            Err(e) => {
                return HealthStatus::unknown(format!(
                    "`{}` is not a valid filter: {e}",
                    spec.expr
                ));
            }
        };
        match filter.matches(cx.host.as_ref()) {
            Ok(true) => HealthStatus::Pass,
            Ok(false) => HealthStatus::fail(format!("`{}` is false", spec.expr)),
            Err(e) => HealthStatus::unknown(format!("`{}` could not be evaluated: {e}", spec.expr)),
        }
    }
}

buckle_core::register_health!(FilterCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::spec;
    use buckle_core::selectors::{HostContext, HostIdentity};
    use buckle_core::testing::{CxBuilder, FakeSystem};
    use rstest::rstest;
    use serde_json::json;

    fn cx_with_facts(facts: serde_json::Value) -> Cx {
        let host = HostContext::new(HostIdentity {
            id: "web-1".into(),
            ..Default::default()
        })
        .with_facts(facts);
        CxBuilder::new(FakeSystem::new()).host(host).build()
    }

    fn facts() -> serde_json::Value {
        json!({
            "hardware": {"disk": {"root": {"free_percent": 42.5}}, "cpus": 4},
            "os": {"family": "debian", "codename": "bookworm"},
            "nomad": {"eligibility": "eligible"},
            "cloud": null,
        })
    }

    #[rstest]
    #[case::enough_disk("host.facts.hardware.disk.root.free_percent > 10", None)]
    #[case::string_match("host.facts.os.family == \"debian\"", None)]
    #[case::combined(
        "host.facts.os.codename == \"bookworm\" && host.facts.hardware.cpus >= 4",
        None
    )]
    #[case::host_id("host.id == \"web-1\"", None)]
    #[case::not_enough_disk("host.facts.hardware.disk.root.free_percent > 50", Some("is false"))]
    #[case::wrong_value("host.facts.nomad.eligibility == \"ineligible\"", Some("is false"))]
    #[case::missing_fact_is_false("host.facts.docker.running == true", Some("is false"))]
    #[case::null_fact_is_false("host.facts.cloud.provider == \"oci\"", Some("is false"))]
    #[tokio::test]
    async fn evaluates_against_the_host_facts(#[case] expr: &str, #[case] failure: Option<&str>) {
        let result = FilterCheck
            .check(&cx_with_facts(facts()), &spec(json!({"expr": expr})))
            .await;
        match failure {
            None => assert_eq!(result, HealthStatus::Pass, "{result:?}"),
            Some(needle) => match result {
                HealthStatus::Fail { reason } => {
                    assert!(reason.contains(needle) && reason.contains(expr), "{reason}")
                }
                other => panic!("expected failure, got {other:?}"),
            },
        }
    }

    #[tokio::test]
    async fn invalid_expressions_are_unknown() {
        let result = FilterCheck
            .check(
                &cx_with_facts(facts()),
                &spec(json!({"expr": "host.facts.x >"})),
            )
            .await;
        match result {
            HealthStatus::Unknown { reason } => {
                assert!(reason.contains("not a valid filter"), "{reason}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_real_hardware_provider_output_satisfies_the_documented_expression() {
        use crate::facts::hardware::HardwareFacts;
        use buckle_core::facts::FactProvider;
        let sys = FakeSystem::new().on_command_ok(
            "df",
            &["-P"],
            "Filesystem Type 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 ext4 30298176 10523212 18467500 37% /\n",
        );
        let collect_cx = CxBuilder::new(sys).build();
        let hardware = HardwareFacts.collect(&collect_cx).await.unwrap();
        let cx = cx_with_facts(json!({"hardware": hardware}));
        let ok = FilterCheck
            .check(
                &cx,
                &spec(json!({"expr": "host.facts.hardware.disk.root.free_percent > 10"})),
            )
            .await;
        assert_eq!(ok, HealthStatus::Pass);
    }
}
