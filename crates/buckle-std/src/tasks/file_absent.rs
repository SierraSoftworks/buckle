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
/// directory is reported as blocked unless `recursive: true`. Because Buckle
/// deleted something that was not its own, there is nothing to undo when the
/// task is later removed from the configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// Absolute path to delete.
    pub path: PathBuf,
    /// Delete directories together with their contents.
    #[serde(default)]
    pub recursive: bool,
}

pub struct FileAbsent;
buckle_core::register_task!(FileAbsent);

impl FileAbsent {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
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
    #[case::file(FakeSystem::new().with_file("/x", "1", FilePerms::default()), false, Class::Change)]
    #[case::empty_dir(FakeSystem::new().with_dir("/x", FilePerms::default()), false, Class::Change)]
    #[case::full_dir_needs_recursive(FakeSystem::new().with_file("/x/a", "1", FilePerms::default()), false, Class::Blocked)]
    #[case::full_dir_recursive(FakeSystem::new().with_file("/x/a", "1", FilePerms::default()), true, Class::Change)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] recursive: bool,
        #[case] expected: Class,
    ) {
        let s: Spec = spec(json!({"path": "/x", "recursive": recursive}));
        assert_eq!(plan_class(&FileAbsent, &cx(&sys), &s).await, expected);
    }

    #[tokio::test]
    async fn apply_deletes_and_second_plan_is_a_noop() {
        let sys = FakeSystem::new().with_file("/x/a", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/x", "recursive": true}));
        let c = cx(&sys);
        let applied = apply(&FileAbsent, &c, &s).await;
        assert!(applied.changed && applied.artefacts.is_empty());
        assert!(!sys.exists("/x"));
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
        let sys = FakeSystem::new().with_file("/x", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/x"}));
        let c = cx(&sys);
        let rec = record("file.absent", "/x", &apply(&FileAbsent, &c, &s).await);
        let out = FileAbsent.remove(&c, &rec).await.unwrap();
        assert!(!out.changed);
    }

    #[tokio::test]
    async fn apply_of_a_blocked_plan_is_a_user_error() {
        let sys = FakeSystem::new().with_file("/x/a", "1", FilePerms::default());
        let s: Spec = spec(json!({"path": "/x"}));
        assert!(
            FileAbsent
                .apply(&cx(&sys), &s, &Planned::NoOp)
                .await
                .is_err()
        );
        assert!(sys.exists("/x/a"));
    }

    use std::path::Path;
}
