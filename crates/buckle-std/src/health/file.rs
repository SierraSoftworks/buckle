//! The `file` health check.

use async_trait::async_trait;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::task::Cx;
use chrono::{DateTime, Utc};
use humantime_serde::re::humantime::format_duration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Checks that a file exists (or does not), is recent and contains a string.
///
/// `exists` defaults to true; with `exists: false` the check passes only when
/// the path is absent. `max_age` fails a file whose modification time is
/// older than that, which is how a backup job's marker file proves the job
/// ran recently. `contains` requires the file's text to contain a string
/// (the whole file is read, so keep it for small files).
pub struct FileCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The path to check.
    pub path: String,
    /// Whether the file must exist; defaults to true.
    #[serde(default = "default_true")]
    pub exists: bool,
    /// The file must have been modified within this long.
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub max_age: Option<Duration>,
    /// A string the file must contain.
    #[serde(default)]
    pub contains: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Fails when `modified` is older than `max_age` at `now`; unknown without a timestamp.
pub(crate) fn check_age(
    path: &str,
    modified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Option<HealthStatus> {
    let Some(modified) = modified else {
        return Some(HealthStatus::unknown(format!(
            "{path} has no modification time"
        )));
    };
    // A file from the future (clock skew) is as fresh as it gets.
    let age = (now - modified).to_std().unwrap_or_default();
    (age > max_age).then(|| {
        HealthStatus::fail(format!(
            "{path} was last modified {} ago (max_age {})",
            format_duration(Duration::from_secs(age.as_secs())),
            format_duration(max_age)
        ))
    })
}

#[async_trait]
impl HealthCheck for FileCheck {
    type Spec = Spec;
    const KIND: &'static str = "file";
    const DESCRIPTION: &'static str =
        "Check that a file exists, is recent (max_age) and contains a string.";

    #[tracing::instrument(name = "health.file", skip_all, fields(path = %spec.path))]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        let path = Path::new(&spec.path);
        let meta = match cx.system.stat(path).await {
            Ok(m) => m,
            Err(e) => {
                return HealthStatus::unknown(format!(
                    "could not stat {}: {}",
                    spec.path,
                    e.description()
                ));
            }
        };
        let Some(meta) = meta else {
            return if spec.exists {
                HealthStatus::fail(format!("{} does not exist", spec.path))
            } else {
                HealthStatus::Pass
            };
        };
        if !spec.exists {
            return HealthStatus::fail(format!("{} exists but should not", spec.path));
        }
        if let Some(max_age) = spec.max_age
            && let Some(status) = check_age(&spec.path, meta.modified, cx.system.now(), max_age)
        {
            return status;
        }
        if let Some(needle) = &spec.contains {
            match cx.system.read(path).await {
                Ok(Some(bytes)) => {
                    if !String::from_utf8_lossy(&bytes).contains(needle.as_str()) {
                        return HealthStatus::fail(format!(
                            "{} does not contain \"{needle}\"",
                            spec.path
                        ));
                    }
                }
                Ok(None) => return HealthStatus::fail(format!("{} does not exist", spec.path)),
                Err(e) => {
                    return HealthStatus::unknown(format!(
                        "could not read {}: {}",
                        spec.path,
                        e.description()
                    ));
                }
            }
        }
        HealthStatus::Pass
    }
}

buckle_core::register_health!(FileCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host() -> FakeSystem {
        FakeSystem::new().with_file(
            "/var/lib/backup/last-success",
            "2026-01-01T00:00:00Z ok snapshot=abc123\n",
            FilePerms::mode(0o644),
        )
    }

    #[rstest]
    #[case::exists(json!({"path": "/var/lib/backup/last-success"}), None)]
    #[case::fresh_enough(json!({"path": "/var/lib/backup/last-success", "max_age": "26h"}), None)]
    #[case::contains(json!({"path": "/var/lib/backup/last-success", "contains": "ok snapshot="}), None)]
    #[case::absent_as_expected(json!({"path": "/etc/nologin", "exists": false}), None)]
    #[case::missing(json!({"path": "/var/lib/backup/missing"}), Some("does not exist"))]
    #[case::present_but_should_not_be(json!({"path": "/var/lib/backup/last-success", "exists": false}), Some("exists but should not"))]
    #[case::missing_text(json!({"path": "/var/lib/backup/last-success", "contains": "failed"}), Some("does not contain \"failed\""))]
    #[tokio::test]
    async fn existence_and_content(
        #[case] spec_json: serde_json::Value,
        #[case] failure: Option<&str>,
    ) {
        let result = FileCheck.check(&cx(&host()), &spec(spec_json)).await;
        match failure {
            None => assert_eq!(result, HealthStatus::Pass, "{result:?}"),
            Some(needle) => match result {
                HealthStatus::Fail { reason } => assert!(reason.contains(needle), "{reason}"),
                other => panic!("expected failure containing {needle:?}, got {other:?}"),
            },
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[rstest]
    #[case::fresh("2026-01-01T00:00:00Z", "2026-01-01T12:00:00Z", 26 * 3600, true)]
    #[case::exactly_at_the_limit("2026-01-01T00:00:00Z", "2026-01-02T02:00:00Z", 26 * 3600, true)]
    #[case::stale("2026-01-01T00:00:00Z", "2026-01-03T00:00:00Z", 26 * 3600, false)]
    #[case::from_the_future("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z", 60, true)]
    fn age_is_measured_from_the_modification_time(
        #[case] modified: &str,
        #[case] now: &str,
        #[case] max_age_secs: u64,
        #[case] pass: bool,
    ) {
        let status = check_age(
            "/p",
            Some(at(modified)),
            at(now),
            Duration::from_secs(max_age_secs),
        );
        assert_eq!(status.is_none(), pass, "{status:?}");
        if let Some(HealthStatus::Fail { reason }) = status {
            assert!(
                reason.contains("last modified 2days ago") && reason.contains("max_age 1day 2h"),
                "{reason}"
            );
        }
    }

    #[test]
    fn age_without_a_timestamp_is_unknown() {
        assert!(matches!(
            check_age(
                "/p",
                None,
                at("2026-01-01T00:00:00Z"),
                Duration::from_secs(1)
            ),
            Some(HealthStatus::Unknown { .. })
        ));
    }

    #[tokio::test]
    async fn max_age_zero_still_passes_for_a_file_modified_right_now() {
        // FakeSystem stamps every file with the current time.
        let result = FileCheck
            .check(
                &cx(&host()),
                &spec(json!({"path": "/var/lib/backup/last-success", "max_age": "0s"})),
            )
            .await;
        assert_eq!(result, HealthStatus::Pass);
    }
}
