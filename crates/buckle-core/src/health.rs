//! Health checks: declared per module, run at the moments a rollout needs an
//! answer (baseline, bake, pre-phase, drift) and never as a separate loop.

use crate::Platforms;
use crate::task::Cx;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

/// When a batch of checks ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthScope {
    /// Before a host's wave: the modules it currently has applied.
    Baseline,
    /// After apply, repeating for the bake window.
    Bake,
    /// Before the next phase starts, on every host already updated.
    Rollout,
    /// Alongside the periodic drift check.
    Drift,
    /// `buckle health` on the command line.
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HealthStatus {
    Pass,
    Fail {
        reason: String,
    },
    /// The check could not run (unsupported platform, missing tool).
    Unknown {
        reason: String,
    },
}

impl HealthStatus {
    pub fn fail(reason: impl Into<String>) -> Self {
        HealthStatus::Fail {
            reason: reason.into(),
        }
    }

    pub fn unknown(reason: impl Into<String>) -> Self {
        HealthStatus::Unknown {
            reason: reason.into(),
        }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, HealthStatus::Pass)
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResult {
    pub module: String,
    pub check: String,
    pub kind: String,
    pub required: bool,
    pub status: HealthStatus,
    pub at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
}

impl HealthResult {
    /// A failing required check that was not waived.
    pub fn counts_as_failure(&self, waived: &[String]) -> bool {
        self.required
            && matches!(self.status, HealthStatus::Fail { .. })
            && !waived
                .iter()
                .any(|w| w == &format!("{}/{}", self.module, self.check))
    }
}

/// A batch of results with its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    pub scope: HealthScope,
    pub results: Vec<HealthResult>,
}

impl HealthReport {
    pub fn healthy(&self, waived: &[String]) -> bool {
        !self.results.iter().any(|r| r.counts_as_failure(waived))
    }

    pub fn failures(&self, waived: &[String]) -> impl Iterator<Item = &HealthResult> {
        self.results
            .iter()
            .filter(move |r| r.counts_as_failure(waived))
    }
}

#[async_trait]
pub trait HealthCheck: Send + Sync + 'static {
    type Spec: DeserializeOwned + Serialize + schemars::JsonSchema + Send + Sync + 'static;
    const KIND: &'static str;
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "";

    async fn check(&self, cx: &Cx, spec: &Self::Spec) -> HealthStatus;
}

/// Type-erased check spec.
pub struct ErasedCheck {
    inner: Box<dyn std::any::Any + Send + Sync>,
    pub json: serde_json::Value,
}

impl ErasedCheck {
    pub fn downcast<S: 'static>(&self) -> Option<&S> {
        self.inner.downcast_ref::<S>()
    }
}

#[async_trait]
pub trait DynHealthCheck: Send + Sync {
    fn kind(&self) -> &'static str;
    fn platforms(&self) -> Platforms;
    fn description(&self) -> &'static str;
    fn schema(&self) -> schemars::Schema;
    fn parse(&self, raw: serde_json::Value) -> crate::Result<ErasedCheck>;
    async fn check(&self, cx: &Cx, spec: &ErasedCheck) -> HealthStatus;
}

#[async_trait]
impl<C: HealthCheck> DynHealthCheck for C {
    fn kind(&self) -> &'static str {
        C::KIND
    }
    fn platforms(&self) -> Platforms {
        C::PLATFORMS
    }
    fn description(&self) -> &'static str {
        C::DESCRIPTION
    }
    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(C::Spec)
    }
    fn parse(&self, raw: serde_json::Value) -> crate::Result<ErasedCheck> {
        let spec: C::Spec = serde_json::from_value(raw.clone()).map_err(|e| {
            crate::error::wrap_user(e, format!("The '{}' health check has an invalid spec.", C::KIND), &[
                "Compare the check's fields against the kind's documentation (`buckle catalog`).",
            ])
        })?;
        Ok(ErasedCheck {
            inner: Box::new(spec),
            json: raw,
        })
    }
    async fn check(&self, cx: &Cx, spec: &ErasedCheck) -> HealthStatus {
        match spec.downcast::<C::Spec>() {
            Some(spec) => HealthCheck::check(self, cx, spec).await,
            None => HealthStatus::unknown("spec of the wrong type (engine bug)"),
        }
    }
}

pub struct HealthRegistration(pub &'static dyn DynHealthCheck);

impl HealthRegistration {
    pub const fn new<C: DynHealthCheck>(c: &'static C) -> Self {
        Self(c)
    }
}

inventory::collect!(HealthRegistration);

pub fn registry() -> &'static HashMap<&'static str, &'static dyn DynHealthCheck> {
    static REGISTRY: LazyLock<HashMap<&'static str, &'static dyn DynHealthCheck>> =
        LazyLock::new(|| {
            let mut map = HashMap::new();
            for reg in inventory::iter::<HealthRegistration> {
                if map.insert(reg.0.kind(), reg.0).is_some() {
                    panic!(
                        "two health check kinds are registered as '{}'",
                        reg.0.kind()
                    );
                }
            }
            map
        });
    &REGISTRY
}

pub fn lookup(kind: &str) -> crate::Result<&'static dyn DynHealthCheck> {
    registry().get(kind).copied().ok_or_else(|| {
        let mut known: Vec<&str> = registry().keys().copied().collect();
        known.sort_unstable();
        crate::error::user(
            format!(
                "'{kind}' is not a health check kind this Buckle knows about. Known kinds: {}.",
                known.join(", ")
            ),
            &["Check the spelling of the check kind in module.yaml."],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn result(module: &str, check: &str, required: bool, status: HealthStatus) -> HealthResult {
        HealthResult {
            module: module.into(),
            check: check.into(),
            kind: "http".into(),
            required,
            status,
            at: chrono::Utc::now(),
            duration_ms: 1,
        }
    }

    #[rstest]
    #[case(true, HealthStatus::Pass, &[], false)]
    #[case(true, HealthStatus::fail("down"), &[], true)]
    #[case(false, HealthStatus::fail("down"), &[], false)]
    #[case(true, HealthStatus::fail("down"), &["nomad/api"], false)]
    #[case(true, HealthStatus::fail("down"), &["nomad/other"], true)]
    #[case(true, HealthStatus::unknown("no tool"), &[], false)]
    fn failure_counting_respects_required_and_waivers(
        #[case] required: bool,
        #[case] status: HealthStatus,
        #[case] waived: &[&str],
        #[case] expected: bool,
    ) {
        let waived: Vec<String> = waived.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            result("nomad", "api", required, status).counts_as_failure(&waived),
            expected
        );
    }

    #[test]
    fn report_health_is_the_absence_of_counted_failures() {
        let report = HealthReport {
            scope: HealthScope::Bake,
            results: vec![
                result("a", "x", true, HealthStatus::Pass),
                result("a", "y", false, HealthStatus::fail("meh")),
            ],
        };
        assert!(report.healthy(&[]));
        assert_eq!(report.failures(&[]).count(), 0);
    }
}
