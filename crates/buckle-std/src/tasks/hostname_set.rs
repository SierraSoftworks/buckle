//! `hostname.set`: set the machine's hostname.

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FilePerms};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::instrument;

/// Sets the host's name, both live and in `/etc/hostname`.
///
/// ```yaml
/// - hostname.set:
///     name: "{{ host.id }}"
/// ```
///
/// The plan compares the running hostname (and `/etc/hostname` when it
/// exists) with `name`. Applying runs `hostnamectl set-hostname` (falling back
/// to `hostname` when `hostnamectl` is missing) and rewrites `/etc/hostname`.
/// `name` must be a valid hostname: letters, digits and `-`/`.`, at most 253
/// characters. Removal does nothing: Buckle does not know what the previous
/// hostname should be, so a hostname is released rather than reverted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
}

pub struct HostnameSet;
buckle_core::register_task!(HostnameSet);

const ETC_HOSTNAME: &str = "/etc/hostname";

fn validate(name: &str) -> Result<()> {
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if name.len() <= 253 && name.split('.').all(label_ok) {
        Ok(())
    } else {
        Err(error::user(
            format!("'{name}' is not a valid hostname."),
            &["Use letters, digits and hyphens; dots separate labels of at most 63 characters."],
        ))
    }
}

#[async_trait]
impl Task for HostnameSet {
    type Spec = Spec;
    const KIND: &'static str = "hostname.set";
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str = "Set the machine's hostname (live and in /etc/hostname).";

    fn key(_spec: &Spec) -> String {
        "hostname".to_owned()
    }

    fn describe(spec: &Spec) -> String {
        format!("hostname {}", spec.name)
    }

    #[instrument(name = "task.hostname.set.plan", skip_all, fields(name = %spec.name))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(&spec.name)?;
        let live = cx.system.hostname();
        let stored = cx
            .system
            .read(Path::new(ETC_HOSTNAME))
            .await?
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned());
        let live_ok = live.eq_ignore_ascii_case(&spec.name);
        let stored_ok = stored
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case(&spec.name));
        Ok(if live_ok && stored_ok {
            Planned::NoOp
        } else {
            Planned::change(Diff::Update {
                summary: format!("hostname {live} → {}", spec.name),
                unified: None,
            })
        })
    }

    #[instrument(name = "task.hostname.set.apply", skip_all, fields(name = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, planned: &Planned) -> Result<Applied> {
        validate(&spec.name)?;
        if matches!(self.plan(cx, spec).await?, Planned::NoOp) && !planned.is_change() {
            return Ok(Applied::unchanged(format!(
                "hostname is already {}",
                spec.name
            )));
        }
        let command = if cx.system.which("hostnamectl").await?.is_some() {
            Command::new("hostnamectl")
                .arg("set-hostname")
                .arg(&spec.name)
        } else {
            hostkit::require_tool(cx, "hostname", "install the `hostname` package").await?;
            Command::new("hostname").arg(&spec.name)
        };
        hostkit::run_checked(cx, command, "Setting the hostname").await?;
        cx.system
            .write_atomic(
                Path::new(ETC_HOSTNAME),
                format!("{}\n", spec.name).as_bytes(),
                &FilePerms::mode(0o644),
            )
            .await?;
        Ok(Applied::changed(format!(
            "set the hostname to {}",
            spec.name
        )))
    }

    fn describe_removal(&self, _record: &ResourceRecord) -> String {
        "nothing: a hostname is released, not reverted".to_owned()
    }

    async fn remove(&self, _cx: &Cx, _record: &ResourceRecord) -> Result<Applied> {
        Ok(Applied::skipped(
            "reverting a hostname is not done; the current hostname was left as it is",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn s(name: &str) -> Spec {
        spec(json!({"name": name}))
    }

    #[rstest]
    #[case::same(FakeSystem::new().with_hostname("web-1"), Class::NoOp)]
    #[case::same_case_insensitively(FakeSystem::new().with_hostname("WEB-1"), Class::NoOp)]
    #[case::different(FakeSystem::new().with_hostname("old"), Class::Change)]
    #[case::stale_etc_hostname(FakeSystem::new().with_hostname("web-1").with_file("/etc/hostname", "old\n", FilePerms::default()), Class::Change)]
    #[case::etc_hostname_matches(FakeSystem::new().with_hostname("web-1").with_file("/etc/hostname", "web-1\n", FilePerms::default()), Class::NoOp)]
    #[tokio::test]
    async fn plan_outcomes(#[case] sys: FakeSystem, #[case] expected: Class) {
        assert_eq!(
            plan_class(&HostnameSet, &cx(&sys), &s("web-1")).await,
            expected
        );
    }

    #[tokio::test]
    async fn apply_uses_hostnamectl_and_writes_etc_hostname() {
        let sys = FakeSystem::new()
            .with_hostname("old")
            .on_command_ok("hostnamectl", &[], "");
        let applied = apply(&HostnameSet, &cx(&sys), &s("web-1")).await;
        assert!(applied.changed);
        assert_eq!(sys.command_lines(), vec!["hostnamectl set-hostname web-1"]);
        assert_eq!(sys.file_string("/etc/hostname").unwrap(), "web-1\n");
        // The post-state (what the host reports after the command) plans as a no-op.
        let after = FakeSystem::new().with_hostname("web-1").with_file(
            "/etc/hostname",
            "web-1\n",
            FilePerms::default(),
        );
        assert_eq!(
            plan_class(&HostnameSet, &cx(&after), &s("web-1")).await,
            Class::NoOp
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_hostname_command() {
        let sys = FakeSystem::new()
            .with_hostname("old")
            .on_command_ok("hostname", &[], "");
        apply(&HostnameSet, &cx(&sys), &s("web-1")).await;
        assert_eq!(sys.command_lines(), vec!["hostname web-1"]);
        let bare = FakeSystem::new().with_hostname("old");
        assert!(
            HostnameSet
                .apply(
                    &cx(&bare),
                    &s("web-1"),
                    &Planned::change(Diff::Opaque {
                        summary: String::new()
                    })
                )
                .await
                .is_err()
        );
    }

    #[rstest]
    #[case("")]
    #[case("-bad")]
    #[case("has space")]
    #[case("under_score")]
    #[case("a..b")]
    #[tokio::test]
    async fn invalid_names_are_user_errors(#[case] name: &str) {
        let sys = FakeSystem::new();
        let err = HostnameSet.plan(&cx(&sys), &s(name)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[tokio::test]
    async fn removal_leaves_the_hostname_alone() {
        let sys = FakeSystem::new();
        let out = HostnameSet
            .remove(
                &cx(&sys),
                &record("hostname.set", "hostname", &Applied::changed("x")),
            )
            .await
            .unwrap();
        assert!(!out.changed && out.summary.contains("not done"));
        assert!(sys.command_lines().is_empty());
    }
}
