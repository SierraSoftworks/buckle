//! `apt.update`: refresh the apt package index when it is stale.

use super::apt_common::apt_get;
use super::sys_exec::{run_checked, state_dir};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use chrono::{DateTime, Utc};
use humantime_serde::re::humantime::format_duration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Runs `apt-get update` when the package index is older than `max_age`.
///
/// `max_age` (default `1h`) is compared with the newest of: the mtime of
/// `/var/lib/apt/lists`, apt's `update-success-stamp`, and Buckle's own stamp
/// written after each run of this task. Use `max_age: 0s` to refresh on every
/// run. Nothing is undone on removal.
pub struct AptUpdate;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Refresh when the index is older than this (`30m`, `1h`, `1d`).
    #[serde(default = "default_max_age", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub max_age: Duration,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            max_age: default_max_age(),
        }
    }
}

fn default_max_age() -> Duration {
    Duration::from_secs(3600)
}

fn own_stamp(cx: &Cx) -> std::path::PathBuf {
    state_dir(cx).join("apt-update.stamp")
}

/// When the apt index was last refreshed, as far as the host can tell.
async fn last_refresh(cx: &Cx) -> Result<Option<DateTime<Utc>>> {
    let mut newest: Option<DateTime<Utc>> = None;
    let mut consider = |t: Option<DateTime<Utc>>| {
        if let Some(t) = t
            && newest.is_none_or(|n| t > n)
        {
            newest = Some(t);
        }
    };
    if let Some(bytes) = cx.system.read(&own_stamp(cx)).await? {
        consider(
            DateTime::parse_from_rfc3339(String::from_utf8_lossy(&bytes).trim())
                .ok()
                .map(|t| t.with_timezone(&Utc)),
        );
    }
    for path in [
        "/var/lib/apt/periodic/update-success-stamp",
        "/var/lib/apt/lists",
    ] {
        if let Some(meta) = cx.system.stat(Path::new(path)).await? {
            consider(meta.modified);
        }
    }
    Ok(newest)
}

#[async_trait]
impl Task for AptUpdate {
    type Spec = Spec;
    const KIND: &'static str = "apt.update";
    const LOCKS: &'static [&'static str] = &["apt"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: buckle_core::model::RemovalPolicy =
        buckle_core::model::RemovalPolicy::Leave;
    const DESCRIPTION: &'static str =
        "Run apt-get update when the package index is older than max_age.";

    fn key(_spec: &Spec) -> String {
        "update".into()
    }

    fn describe(spec: &Spec) -> String {
        format!(
            "apt index refreshed within {}",
            format_duration(spec.max_age)
        )
    }

    #[tracing::instrument(name = "task.apt.update.plan", skip_all)]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let now = cx.system.now();
        let age = last_refresh(cx)
            .await?
            .map(|t| (now - t).to_std().unwrap_or_default());
        match age {
            Some(age) if age < spec.max_age => Ok(Planned::NoOp),
            Some(age) => Ok(Planned::change(Diff::Opaque {
                summary: format!(
                    "apt-get update (index is {} old, max {})",
                    format_duration(Duration::from_secs(age.as_secs())),
                    format_duration(spec.max_age)
                ),
            })),
            None => Ok(Planned::change(Diff::Opaque {
                summary: "apt-get update (the index has never been refreshed)".into(),
            })),
        }
    }

    #[tracing::instrument(name = "task.apt.update.apply", skip_all)]
    async fn apply(&self, cx: &Cx, _spec: &Spec, _planned: &Planned) -> Result<Applied> {
        run_checked(cx, apt_get("update", false)).await?;
        cx.system
            .write_atomic(
                &own_stamp(cx),
                cx.system.now().to_rfc3339().as_bytes(),
                &FilePerms::mode(0o644),
            )
            .await?;
        Ok(Applied::changed("refreshed the apt package index"))
    }

    fn describe_removal(&self, _record: &buckle_core::state::ResourceRecord) -> String {
        "nothing (the apt index is left as it is)".into()
    }
}

buckle_core::register_task!(AptUpdate);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host() -> FakeSystem {
        FakeSystem::new().on_command_ok("apt-get", &["update"], "Reading package lists...\n")
    }

    #[rstest]
    #[case::never_refreshed(None, "1h", true)]
    #[case::fresh_own_stamp(Some("2025-12-31T23:30:00Z"), "1h", false)]
    #[case::old_own_stamp(Some("2025-12-31T20:00:00Z"), "1h", true)]
    #[case::long_max_age(Some("2025-12-30T00:00:00Z"), "7d", false)]
    #[tokio::test]
    async fn plan_follows_the_stamp_age(
        #[case] stamp: Option<&str>,
        #[case] max_age: &str,
        #[case] change: bool,
    ) {
        let mut sys = host();
        if let Some(stamp) = stamp {
            sys = sys.with_file(
                "/var/lib/buckle/apt-update.stamp",
                stamp,
                FilePerms::default(),
            );
        }
        let cx = cx(&sys);
        let planned = AptUpdate
            .plan(&cx, &spec(json!({"max_age": max_age})))
            .await
            .unwrap();
        assert_eq!(planned.is_change(), change, "{planned:?}");
    }

    #[tokio::test]
    async fn apt_own_bookkeeping_counts_as_a_refresh() {
        // FakeSystem reports every existing path as modified "now".
        let sys = host().with_dir("/var/lib/apt/lists", FilePerms::default());
        let cx = cx(&sys);
        assert_eq!(
            AptUpdate.plan(&cx, &Spec::default()).await.unwrap(),
            Planned::NoOp
        );
        // A zero max_age always refreshes.
        let always = AptUpdate
            .plan(&cx, &spec(json!({"max_age": "0s"})))
            .await
            .unwrap();
        assert!(always.is_change());
    }

    #[tokio::test]
    async fn apply_updates_stamps_and_converges() {
        let sys = host();
        let cx = cx(&sys);
        let s = Spec::default();
        assert!(AptUpdate.plan(&cx, &s).await.unwrap().is_change());
        let applied = AptUpdate.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(applied.changed);
        assert!(sys.command_lines()[0].starts_with("apt-get update"));
        assert_eq!(
            sys.file_string("/var/lib/buckle/apt-update.stamp")
                .as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(AptUpdate.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn removal_has_nothing_to_undo() {
        let sys = host();
        let cx = cx(&sys);
        let applied = AptUpdate
            .apply(&cx, &Spec::default(), &Planned::NoOp)
            .await
            .unwrap();
        let record = crate::tasks::sys_exec::testkit::record_of("apt.update", "update", &applied);
        let removed = AptUpdate.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert_eq!(sys.command_lines().len(), 1);
    }

    #[test]
    fn default_spec_is_one_hour() {
        let s: Spec = spec(json!({}));
        assert_eq!(s.max_age, Duration::from_secs(3600));
    }
}
