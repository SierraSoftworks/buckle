//! `cron.job`: a cron job in a Buckle-owned `/etc/cron.d` file.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Manages a cron job as its own file in `/etc/cron.d`.
///
/// ```yaml
/// - cron.job:
///     name: backup-nightly            # file name: letters, digits, '-' and '_' only
///     user: root                      # default: root
///     schedule: "30 2 * * *"          # five fields, or @hourly @daily @weekly @monthly @yearly @reboot
///     command: /usr/local/bin/backup --quiet
///     state: present                  # or absent
/// ```
///
/// The file `/etc/cron.d/<name>` (mode 0644, root) is owned entirely by
/// Buckle. Unescaped `%` characters in `command` are escaped for you, because
/// cron would otherwise treat them as newlines. `command` must be one line.
/// Removal deletes the file if it still has the content Buckle wrote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    pub name: String,
    #[serde(default = "root")]
    pub user: String,
    pub schedule: String,
    pub command: String,
    #[serde(default)]
    pub state: Ensure,
}

fn root() -> String {
    "root".into()
}

pub struct CronJob;
buckle_core::register_task!(CronJob);

fn path(spec: &Spec) -> PathBuf {
    PathBuf::from("/etc/cron.d").join(&spec.name)
}

fn validate(spec: &Spec) -> Result<()> {
    if spec.name.is_empty()
        || !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(error::user(
            format!("'{}' is not a usable cron job name.", spec.name),
            &[
                "cron ignores files in /etc/cron.d whose names contain dots; use letters, digits, '-' and '_'.",
            ],
        ));
    }
    super::user_group::valid_name(&spec.user)?;
    let schedule = spec.schedule.trim();
    let macros = [
        "@reboot",
        "@yearly",
        "@annually",
        "@monthly",
        "@weekly",
        "@daily",
        "@midnight",
        "@hourly",
    ];
    if !(macros.contains(&schedule) || schedule.split_whitespace().count() == 5) {
        return Err(error::user(
            format!("'{schedule}' is not a cron schedule."),
            &[
                "Use five fields (`30 2 * * *`) or one of @hourly, @daily, @weekly, @monthly, @yearly, @reboot.",
            ],
        ));
    }
    if spec.command.trim().is_empty() || spec.command.contains('\n') {
        return Err(error::user(
            "A cron job's `command` must be a single, non-empty line.",
            &["Put longer scripts in a file and run that."],
        ));
    }
    Ok(())
}

fn escape_percent(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut prev = ' ';
    for c in command.chars() {
        if c == '%' && prev != '\\' {
            out.push('\\');
        }
        out.push(c);
        prev = c;
    }
    out
}

fn render(spec: &Spec) -> String {
    format!(
        "# Managed by Buckle (cron.job {}). Local changes will be overwritten.\n{} {} {}\n",
        spec.name,
        spec.schedule
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        spec.user,
        escape_percent(spec.command.trim()),
    )
}

#[async_trait]
impl Task for CronJob {
    type Spec = Spec;
    const KIND: &'static str = "cron.job";
    const PLATFORMS: Platforms = Platforms::UNIX;
    const DESCRIPTION: &'static str = "Manage a cron job as a Buckle-owned file in /etc/cron.d.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!("cron job {} ({})", spec.name, spec.schedule)
    }

    #[instrument(name = "task.cron.job.plan", skip_all, fields(job = %spec.name, state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(spec)?;
        let file = path(spec);
        let current = cx.system.read(&file).await?;
        Ok(match (spec.state, current) {
            (Ensure::Absent, None) => Planned::NoOp,
            (Ensure::Absent, Some(_)) => Planned::change(Diff::Delete {
                summary: format!("delete {}", file.display()),
            }),
            (Ensure::Present, None) => Planned::change(Diff::Create {
                summary: format!("create {}", file.display()),
                preview: Some(buckle_core::secrets::Redacted::plain(render(spec))),
            }),
            (Ensure::Present, Some(bytes)) => {
                let wanted = render(spec);
                let perms = match cx.system.stat(&file).await? {
                    Some(m) => hostkit::perm_changes(&m, &FilePerms::mode(0o644)),
                    None => vec![],
                };
                if bytes == wanted.as_bytes() && perms.is_empty() {
                    Planned::NoOp
                } else {
                    hostkit::text_diff_planned(
                        cx,
                        &file,
                        format!("update {}", file.display()),
                        &String::from_utf8_lossy(&bytes),
                        &wanted,
                    )
                }
            }
        })
    }

    #[instrument(name = "task.cron.job.apply", skip_all, fields(job = %spec.name, state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        validate(spec)?;
        let file = path(spec);
        match spec.state {
            Ensure::Absent => {
                let removed = cx.system.remove(&file, false).await?;
                Ok(if removed {
                    Applied::changed(format!("deleted {}", file.display()))
                } else {
                    Applied::unchanged(format!("{} was already absent", file.display()))
                })
            }
            Ensure::Present => {
                let content = render(spec);
                let unchanged = cx
                    .system
                    .read(&file)
                    .await?
                    .is_some_and(|b| b == content.as_bytes())
                    && match cx.system.stat(&file).await? {
                        Some(m) => hostkit::perm_changes(&m, &FilePerms::mode(0o644)).is_empty(),
                        None => false,
                    };
                if !unchanged {
                    cx.system
                        .write_atomic(
                            &file,
                            content.as_bytes(),
                            &FilePerms {
                                mode: Some(0o644),
                                owner: Some("root".into()),
                                group: None,
                            },
                        )
                        .await?;
                }
                Ok(hostkit::written(
                    format!(
                        "{} {}",
                        if unchanged { "kept" } else { "wrote" },
                        file.display()
                    ),
                    &file,
                    Digest::of(content.as_bytes()),
                    Some(buckle_core::secrets::Redacted::plain(content)),
                    !unchanged,
                ))
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!("delete /etc/cron.d/{} if it is unchanged", record.key.key)
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        hostkit::remove_recorded_file(cx, record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw =
            json!({"name": "backup", "schedule": "30 2 * * *", "command": "/usr/bin/backup"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[rstest]
    #[case::missing(FakeSystem::new(), json!({}), Class::Change)]
    #[case::current(FakeSystem::new().with_file("/etc/cron.d/backup", render(&s(json!({}))), FilePerms::mode(0o644)), json!({}), Class::NoOp)]
    #[case::schedule_changed(FakeSystem::new().with_file("/etc/cron.d/backup", render(&s(json!({}))), FilePerms::mode(0o644)), json!({"schedule": "@daily"}), Class::Change)]
    #[case::hand_edited(FakeSystem::new().with_file("/etc/cron.d/backup", "* * * * * root evil\n", FilePerms::mode(0o644)), json!({}), Class::Change)]
    #[case::bad_mode(FakeSystem::new().with_file("/etc/cron.d/backup", render(&s(json!({}))), FilePerms::mode(0o666)), json!({}), Class::Change)]
    #[case::absent_present(FakeSystem::new().with_file("/etc/cron.d/backup", "x", FilePerms::default()), json!({"state": "absent"}), Class::Change)]
    #[case::absent_missing(FakeSystem::new(), json!({"state": "absent"}), Class::NoOp)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        assert_eq!(plan_class(&CronJob, &cx(&sys), &s(extra)).await, expected);
    }

    #[tokio::test]
    async fn writes_a_cron_d_file_and_is_idempotent() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let spec = s(json!({"user": "app", "command": "date +%F > /tmp/d"}));
        let applied = apply(&CronJob, &c, &spec).await;
        assert!(applied.changed);
        let written = sys.file_string("/etc/cron.d/backup").unwrap();
        assert!(
            written.ends_with("30 2 * * * app date +\\%F > /tmp/d\n"),
            "{written}"
        );
        assert_eq!(plan_class(&CronJob, &c, &spec).await, Class::NoOp);
        // Updating the schedule rewrites the file.
        let changed =
            s(json!({"user": "app", "command": "date +%F > /tmp/d", "schedule": "0 * * * *"}));
        apply(&CronJob, &c, &changed).await;
        assert!(
            sys.file_string("/etc/cron.d/backup")
                .unwrap()
                .contains("\n0 * * * * app")
        );
    }

    #[test]
    fn percent_escaping_is_not_applied_twice() {
        assert_eq!(escape_percent("a%b"), "a\\%b");
        assert_eq!(escape_percent("a\\%b"), "a\\%b");
    }

    #[tokio::test]
    async fn absent_and_removal() {
        let sys = FakeSystem::new().with_file("/etc/cron.d/backup", "x", FilePerms::default());
        let c = cx(&sys);
        apply(&CronJob, &c, &s(json!({"state": "absent"}))).await;
        assert!(!sys.exists("/etc/cron.d/backup"));

        let rec = record(
            "cron.job",
            "backup",
            &apply(&CronJob, &c, &s(json!({}))).await,
        );
        put(&sys, "/etc/cron.d/backup", "# hand edit\n").await;
        let out = CronJob.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("changed outside Buckle"));
        put(&sys, "/etc/cron.d/backup", &render(&s(json!({})))).await;
        assert!(CronJob.remove(&c, &rec).await.unwrap().changed);
        assert!(!sys.exists("/etc/cron.d/backup"));
    }

    #[rstest]
    #[case::dotted_name(json!({"name": "my.job"}))]
    #[case::empty_name(json!({"name": ""}))]
    #[case::bad_schedule(json!({"schedule": "every day"}))]
    #[case::four_fields(json!({"schedule": "* * * *"}))]
    #[case::multiline(json!({"command": "a\nb"}))]
    #[case::empty_command(json!({"command": " "}))]
    #[tokio::test]
    async fn invalid_jobs_are_user_errors(#[case] extra: serde_json::Value) {
        let sys = FakeSystem::new();
        let err = CronJob.plan(&cx(&sys), &s(extra)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[test]
    fn macros_are_valid_schedules() {
        assert!(validate(&s(json!({"schedule": "@daily"}))).is_ok());
        assert!(validate(&s(json!({"schedule": "*/5 * * * *"}))).is_ok());
    }
}
