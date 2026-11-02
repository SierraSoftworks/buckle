//! `file.absent`: ensure a path does not exist.

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FileKind;
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Ensures a file, symlink or directory does not exist.
///
/// ```yaml
/// - file.absent:
///     path: /etc/motd.d/10-legacy
///     recursive: false      # required to delete a non-empty directory
/// ```
///
/// A symlink is removed as a link and its target is left alone. A non-empty
/// directory is reported as blocked unless `recursive: true`. A recursive
/// delete is refused for `/`, for any top-level directory (`/etc`, `/var`,
/// `/home`, ...), for well-known system directories such as `/usr/lib` or
/// `/var/lib`, for a path containing `..`, and for a path that is empty or
/// has surrounding whitespace (the usual sign of a template that rendered
/// nothing). Because Buckle
/// deleted something that was not its own, there is nothing to undo when the
/// task is later removed from the configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path to delete.
    pub path: PathBuf,
    /// Delete directories together with their contents.
    #[serde(default)]
    pub recursive: bool,
}

pub struct FileAbsent;
buckle_core::register_task!(FileAbsent);

/// System directories (beyond `/` and every top-level directory) that a
/// recursive delete must never target.
const PROTECTED: &[&str] = &[
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/local",
    "/usr/share",
    "/usr/include",
    "/var/lib",
    "/var/log",
    "/var/cache",
    "/var/spool",
    "/var/backups",
    "/etc/ssh",
    "/etc/systemd",
    "/etc/default",
    "/etc/apt",
    "/etc/sudoers.d",
    "/etc/ssl",
    "/etc/pam.d",
    "/etc/security",
    "/etc/network",
    "/etc/netplan",
    "/boot/efi",
    "/lib/modules",
];

/// Refuses recursive deletes of paths whose removal would wreck the host.
fn check_recursive_target(path: &std::path::Path) -> Result<()> {
    use std::path::Component;
    let text = path.to_string_lossy();
    let refuse = |why: &str| {
        Err(buckle_core::error::user(
            format!("file.absent refuses to delete '{text}' recursively: {why}."),
            &[
                "Check the `path` (and the variables it is rendered from).",
                "Delete a more specific path, such as the application's own subdirectory.",
            ],
        ))
    };
    if text.trim().is_empty() {
        return refuse("the path is empty");
    }
    if text.trim() != text {
        return refuse("the path has leading or trailing whitespace");
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return refuse("the path contains `.` or `..`");
    }
    let normal: Vec<_> = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect();
    if normal.len() <= 1 {
        return refuse("it is the root or a top-level directory");
    }
    let trimmed = text.trim_end_matches('/');
    if PROTECTED.contains(&trimmed) {
        return refuse("it is a system directory");
    }
    Ok(())
}

impl FileAbsent {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        if spec.recursive {
            check_recursive_target(&spec.path)?;
        }
        hostkit::require_absolute(&spec.path, "path")?;
        let Some(meta) = cx.system.stat(&spec.path).await? else {
            return Ok(Planned::NoOp);
        };
        let what = match meta.kind {
            FileKind::Directory => {
                let entries = cx.system.list_dir(&spec.path).await?.unwrap_or_default();
                if !entries.is_empty() && !spec.recursive {
                    return Ok(Planned::blocked(format!(
                        "{} is a directory with {} entries; set `recursive: true` to delete it with its contents",
                        spec.path.display(),
                        entries.len()
                    )));
                }
                "directory"
            }
            FileKind::Symlink => "symlink",
            _ => "file",
        };
        Ok(Planned::change(Diff::Delete {
            summary: format!("delete {what} {}", spec.path.display()),
        }))
    }
}

#[async_trait]
impl Task for FileAbsent {
    type Spec = Spec;
    const KIND: &'static str = "file.absent";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Ensure a file, symlink or directory does not exist.";

    fn key(spec: &Spec) -> String {
        spec.path.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        format!("{} absent", spec.path.display())
    }

    #[instrument(name = "task.file.absent.plan", skip_all, fields(path = %spec.path.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        self.inspect(cx, spec).await
    }

    #[instrument(name = "task.file.absent.apply", skip_all, fields(path = %spec.path.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        match self.inspect(cx, spec).await? {
            Planned::Change { .. } => {
                cx.system.remove(&spec.path, spec.recursive).await?;
                Ok(Applied::changed(format!("deleted {}", spec.path.display())))
            }
            Planned::Blocked { reason } => Err(buckle_core::error::user(
                format!("Cannot delete {}: {reason}.", spec.path.display()),
                &["Set `recursive: true` if deleting the directory's contents is intended."],
            )),
            Planned::NoOp => Ok(Applied::unchanged(format!(
                "{} was already absent",
                spec.path.display()
            ))),
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!("nothing: {} was deleted, not created", record.key.key)
    }

    async fn remove(&self, _cx: &Cx, _record: &ResourceRecord) -> Result<Applied> {
        Ok(Applied::skipped(
            "file.absent has nothing to undo; deleted files are not restored",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::missing(FakeSystem::new(), false, Class::NoOp)]
    #[case::file(FakeSystem::new().with_file("/srv/x", "1", FilePerms::default()), false, Class::Change)]
    #[case::empty_dir(FakeSystem::new().with_dir("/srv/x", FilePerms::default()), false, Class::Change)]
    #[case::full_dir_needs_recursive(FakeSystem::new().with_file("/srv/x/a", "1", FilePerms::default()), false, Class::Blocked)]
    #[case::full_dir_recursive(FakeSystem::new().with_file("/srv/x/a", "1", FilePerms::default()), true, Class::Change)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] recursive: bool,
        #[case] expected: Class,
    ) {
        let s: Spec = spec(json!({"path": "/srv/x", "recursive": recursive}));
        assert_eq!(plan_class(&FileAbsent, &cx(&sys), &s).await, expected);
    }

    #[tokio::test]
    async fn apply_deletes_and_second_plan_is_a_noop() {
        let sys = FakeSystem::new().with_file("/srv/x/a", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/srv/x", "recursive": true}));
        let c = cx(&sys);
        let applied = apply(&FileAbsent, &c, &s).await;
        assert!(applied.changed && applied.artefacts.is_empty());
        assert!(!sys.exists("/srv/x"));
        assert_eq!(plan_class(&FileAbsent, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn symlinks_are_removed_without_touching_their_target() {
        let sys = FakeSystem::new().with_file("/real", "1", FilePerms::default());
        buckle_core::system::System::symlink(&sys, Path::new("/real"), Path::new("/link"))
            .await
            .unwrap();
        let s: Spec = spec(json!({"path": "/link"}));
        apply(&FileAbsent, &cx(&sys), &s).await;
        assert!(!sys.exists("/link") && sys.exists("/real"));
    }

    #[tokio::test]
    async fn removal_does_nothing() {
        let sys = FakeSystem::new().with_file("/srv/x", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/srv/x"}));
        let c = cx(&sys);
        let rec = record("file.absent", "/srv/x", &apply(&FileAbsent, &c, &s).await);
        let out = FileAbsent.remove(&c, &rec).await.unwrap();
        assert!(!out.changed);
    }

    #[tokio::test]
    async fn apply_of_a_blocked_plan_is_a_user_error() {
        let sys = FakeSystem::new().with_file("/srv/x/a", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/srv/x"}));
        assert!(
            FileAbsent
                .apply(&cx(&sys), &s, &Planned::NoOp)
                .await
                .is_err()
        );
        assert!(sys.exists("/srv/x/a"));
    }

    use std::path::Path;

    #[rstest]
    #[case::root("/")]
    #[case::root_slashes("//")]
    #[case::etc("/etc")]
    #[case::etc_slash("/etc/")]
    #[case::home("/home")]
    #[case::var_lib("/var/lib")]
    #[case::usr_lib("/usr/lib/")]
    #[case::empty("")]
    #[case::whitespace("   ")]
    #[case::trailing_space("/srv/app ")]
    #[case::dotdot("/srv/app/../..")]
    #[tokio::test]
    async fn recursive_deletes_of_dangerous_paths_are_refused(#[case] path: &str) {
        let sys = FakeSystem::new()
            .with_dir("/etc", FilePerms::mode(0o755))
            .with_file("/etc/passwd", "root", FilePerms::mode(0o644));
        let spec: Spec = spec(json!({"path": path, "recursive": true}));
        let err = FileAbsent.plan(&cx(&sys), &spec).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User), "{path}");
        assert!(
            FileAbsent
                .apply(&cx(&sys), &spec, &Planned::NoOp)
                .await
                .is_err()
        );
        assert!(sys.exists("/etc/passwd"));
    }

    #[tokio::test]
    async fn recursive_deletes_of_application_paths_still_work() {
        let sys = FakeSystem::new().with_file("/srv/app/cache/x", "x", FilePerms::default());
        let spec: Spec = spec(json!({"path": "/srv/app/cache", "recursive": true}));
        apply(&FileAbsent, &cx(&sys), &spec).await;
        assert!(!sys.exists("/srv/app/cache"));
    }
}
