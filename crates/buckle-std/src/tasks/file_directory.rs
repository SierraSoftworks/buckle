//! `file.directory`: ensure a directory exists with the given permissions.

use super::hostkit::{self, describe_perms};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Ensures a directory exists with the given permissions.
///
/// ```yaml
/// - file.directory:
///     path: /var/lib/nomad
///     mode: "0750"
///     owner: nomad
///     group: nomad
///     recurse: false
/// ```
///
/// Missing parent directories are created too. With `recurse: true` the
/// permissions are also applied to everything already inside the directory
/// (the mode applies to files as well as directories, so prefer setting
/// `owner` and `group` when recursing). `recurse` is about permissions only.
///
/// Removal deletes the directory only if Buckle created it and it is empty
/// (files that other tasks wrote into it are removed by those tasks first,
/// since removals run in reverse order). Contents Buckle did not create, such
/// as the data a service keeps in `/var/lib/nomad`, are never deleted, and a
/// directory that existed before Buckle managed it is never deleted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the directory.
    pub path: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Apply the permissions to existing contents as well (never affects removal).
    #[serde(default)]
    pub recurse: bool,
}

pub struct FileDirectory;
buckle_core::register_task!(FileDirectory);

/// Recorded by earlier versions to make removal recursive; now ignored, and
/// dropped from the record on the next apply.
const LEGACY_RECURSIVE_MARKER: &str = "file.directory: remove recursively";

/// Everything beneath `root` whose permissions differ from `want`, as `(path, changes)`.
async fn drifted_descendants(
    cx: &Cx,
    root: &Path,
    want: &FilePerms,
) -> Result<Vec<(PathBuf, Vec<String>)>> {
    let mut drifted = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Some(children) = cx.system.list_dir(&dir).await? else {
            continue;
        };
        for child in children {
            let Some(meta) = cx.system.stat(&child).await? else {
                continue;
            };
            if meta.kind == FileKind::Symlink {
                continue;
            }
            let changes = hostkit::perm_changes(&meta, want);
            if !changes.is_empty() {
                drifted.push((child.clone(), changes));
            }
            if meta.kind == FileKind::Directory {
                stack.push(child);
            }
        }
    }
    drifted.sort();
    Ok(drifted)
}

fn is_legacy_marker(artefact: &Artefact) -> bool {
    matches!(artefact, Artefact::Other { description } if description == LEGACY_RECURSIVE_MARKER)
}

struct Inspection {
    planned: Planned,
    exists: Option<FileMeta>,
    drifted: Vec<(PathBuf, Vec<String>)>,
}

impl FileDirectory {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.path, "path")?;
        let path = spec.path.display();
        let Some(meta) = cx.system.stat(&spec.path).await? else {
            return Ok(Inspection {
                planned: Planned::change(Diff::Create {
                    summary: format!("create directory {path} ({})", describe_perms(&spec.perms)),
                    preview: None,
                }),
                exists: None,
                drifted: Vec::new(),
            });
        };
        if meta.kind != FileKind::Directory {
            return Err(error::user(
                format!("{path} exists but is not a directory."),
                &["Remove or rename the existing file, or use a different `path`."],
            ));
        }
        let changes = hostkit::perm_changes(&meta, &spec.perms);
        let drifted = if spec.recurse && hostkit::has_perms(&spec.perms) {
            drifted_descendants(cx, &spec.path, &spec.perms).await?
        } else {
            Vec::new()
        };
        let planned = if changes.is_empty() && drifted.is_empty() {
            Planned::NoOp
        } else {
            let mut parts = changes;
            if !drifted.is_empty() {
                parts.push(format!("{} entries beneath it", drifted.len()));
            }
            Planned::change(Diff::Update {
                summary: format!("{path}: {}", parts.join(", ")),
                unified: None,
            })
        };
        Ok(Inspection {
            planned,
            exists: Some(meta),
            drifted,
        })
    }
}

#[async_trait]
impl Task for FileDirectory {
    type Spec = Spec;
    const KIND: &'static str = "file.directory";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Ensure a directory exists with the given mode, owner and group (optionally recursively).";

    fn key(spec: &Spec) -> String {
        spec.path.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        if perms.is_empty() {
            format!("{}/", spec.path.display())
        } else {
            format!("{}/ ({perms})", spec.path.display())
        }
    }

    #[instrument(name = "task.file.directory.plan", skip_all, fields(path = %spec.path.display(), recurse = spec.recurse))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.file.directory.apply", skip_all, fields(path = %spec.path.display(), recurse = spec.recurse))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let inspection = self.inspect(cx, spec).await?;
        if !inspection.planned.is_change() {
            return Ok(hostkit::ownership::carry_forward_if(
                cx,
                Applied::unchanged(format!("{} already up to date", spec.path.display())),
                |a| !is_legacy_marker(a),
            ));
        }
        let applied;
        if inspection.exists.is_none() {
            cx.system.mkdir_all(&spec.path, &spec.perms).await?;
            applied = Applied::changed(format!("created directory {}", spec.path.display()))
                .with_artefact(Artefact::Directory {
                    path: spec.path.clone(),
                });
        } else {
            cx.system.set_perms(&spec.path, &spec.perms).await?;
            // A directory Buckle created earlier stays Buckle's after a permission change.
            applied = hostkit::ownership::carry_forward_if(
                cx,
                Applied::changed(format!("updated {}", spec.path.display())),
                |a| !is_legacy_marker(a),
            );
        }
        for (path, _) in &inspection.drifted {
            cx.system.set_perms(path, &spec.perms).await?;
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        if record
            .artefacts
            .iter()
            .any(|a| matches!(a, Artefact::Directory { .. }))
        {
            format!(
                "delete directory {} if Buckle created it and it is empty",
                record.key.key
            )
        } else {
            format!(
                "leave directory {} (it existed before Buckle managed it)",
                record.key.key
            )
        }
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(path) = record.artefacts.iter().find_map(|a| match a {
            Artefact::Directory { path } => Some(path.clone()),
            _ => None,
        }) else {
            return Ok(Applied::skipped(
                "the directory existed before Buckle managed it, left in place",
            ));
        };
        let Some(meta) = cx.system.stat(&path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} was already absent",
                path.display()
            )));
        };
        if meta.kind != FileKind::Directory {
            return Ok(Applied::skipped(format!(
                "{} is no longer a directory, left in place",
                path.display()
            )));
        }
        // Never recursive: whatever is still inside was not created by this
        // task, and may be data that a service or an operator put there.
        let entries = cx.system.list_dir(&path).await?.unwrap_or_default();
        if !entries.is_empty() {
            return Ok(Applied::skipped(format!(
                "{} is not empty ({} entries Buckle did not create), left in place",
                path.display(),
                entries.len()
            )));
        }
        cx.system.remove(&path, false).await?;
        Ok(Applied::changed(format!(
            "deleted directory {}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::missing(FakeSystem::new(), json!({}), Class::Change)]
    #[case::present(FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o755)), json!({"mode": "0755"}), Class::NoOp)]
    #[case::wrong_mode(FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o700)), json!({"mode": "0755"}), Class::Change)]
    #[case::mode_unspecified(FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o700)), json!({}), Class::NoOp)]
    #[case::recursive_drift(
        FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o755)).with_file("/srv/app/x", "x", FilePerms::mode(0o600)),
        json!({"mode": "0755", "recurse": true}),
        Class::Change
    )]
    #[case::recursive_clean(
        FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o755)).with_file("/srv/app/x", "x", FilePerms::mode(0o755)),
        json!({"mode": "0755", "recurse": true}),
        Class::NoOp
    )]
    #[case::non_recursive_ignores_children(
        FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o755)).with_file("/srv/app/x", "x", FilePerms::mode(0o600)),
        json!({"mode": "0755"}),
        Class::NoOp
    )]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        let mut raw = json!({"path": "/srv/app"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let s: Spec = spec(raw);
        assert_eq!(plan_class(&FileDirectory, &cx(&sys), &s).await, expected);
    }

    #[tokio::test]
    async fn apply_creates_with_perms_and_is_idempotent() {
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"path": "/srv/app/data", "mode": "0750", "owner": "app"}));
        let c = cx(&sys);
        let applied = apply(&FileDirectory, &c, &s).await;
        assert!(applied.changed);
        assert!(matches!(applied.artefacts[0], Artefact::Directory { .. }));
        let meta = buckle_core::system::System::stat(&sys, Path::new("/srv/app/data"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (meta.kind, meta.mode, meta.owner.as_deref()),
            (FileKind::Directory, Some(0o750), Some("app"))
        );
        assert_eq!(plan_class(&FileDirectory, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn recursive_apply_fixes_the_whole_tree() {
        let sys = FakeSystem::new()
            .with_dir("/srv/app", FilePerms::mode(0o755))
            .with_file("/srv/app/sub/x", "x", FilePerms::mode(0o600));
        let s: Spec = spec(json!({"path": "/srv/app", "owner": "app", "recurse": true}));
        let c = cx(&sys);
        apply(&FileDirectory, &c, &s).await;
        let meta = buckle_core::system::System::stat(&sys, Path::new("/srv/app/sub/x"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.owner.as_deref(), Some("app"));
        assert_eq!(plan_class(&FileDirectory, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn a_file_in_the_way_is_a_user_error() {
        let sys = FakeSystem::new().with_file("/srv/app", "x", FilePerms::default());
        let s: Spec = spec(json!({"path": "/srv/app"}));
        assert!(FileDirectory.plan(&cx(&sys), &s).await.is_err());
    }

    #[tokio::test]
    async fn removal_deletes_only_empty_directories_buckle_created() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let s: Spec = spec(json!({"path": "/srv/app"}));
        let rec = record(
            "file.directory",
            "/srv/app",
            &apply(&FileDirectory, &c, &s).await,
        );

        put(&sys, "/srv/app/x", "x").await;
        let out = FileDirectory.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("not empty"));
        assert!(sys.exists("/srv/app/x"));

        sys.remove_for_test("/srv/app/x").await;
        assert!(FileDirectory.remove(&c, &rec).await.unwrap().changed);
        assert!(!sys.exists("/srv/app"));

        // `recurse` is about permissions: a service's data is never deleted.
        let s: Spec = spec(json!({"path": "/var/lib/nomad", "owner": "nomad", "recurse": true}));
        let rec = record(
            "file.directory",
            "/var/lib/nomad",
            &apply(&FileDirectory, &c, &s).await,
        );
        put(&sys, "/var/lib/nomad/server/raft/raft.db", "raft").await;
        put(&sys, "/var/lib/nomad/client/state.db", "state").await;
        assert!(!FileDirectory.describe_removal(&rec).contains("contents"));
        let out = FileDirectory.remove(&c, &rec).await.unwrap();
        assert!(
            !out.changed && out.summary.contains("not empty"),
            "{}",
            out.summary
        );
        assert!(sys.exists("/var/lib/nomad/server/raft/raft.db"));
        assert!(sys.exists("/var/lib/nomad/client/state.db"));

        // A record from an earlier version that asked for recursive removal is not obeyed.
        let mut legacy = rec.clone();
        legacy.artefacts.push(Artefact::Other {
            description: LEGACY_RECURSIVE_MARKER.into(),
        });
        assert!(!FileDirectory.remove(&c, &legacy).await.unwrap().changed);
        assert!(sys.exists("/var/lib/nomad/server/raft/raft.db"));
    }

    #[tokio::test]
    async fn directories_that_pre_existed_are_never_deleted() {
        let sys = FakeSystem::new().with_dir("/srv/app", FilePerms::mode(0o700));
        let c = cx(&sys);
        let s: Spec = spec(json!({"path": "/srv/app", "mode": "0755"}));
        let rec = record(
            "file.directory",
            "/srv/app",
            &apply(&FileDirectory, &c, &s).await,
        );
        let out = FileDirectory.remove(&c, &rec).await.unwrap();
        assert!(!out.changed);
        assert!(sys.exists("/srv/app"));
    }

    #[tokio::test]
    async fn a_permission_update_keeps_the_directory_buckle_created() {
        let sys = FakeSystem::new();
        let created = apply(
            &FileDirectory,
            &cx(&sys),
            &spec(json!({"path": "/srv/app"})),
        )
        .await;
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .previous(record("file.directory", "/srv/app", &created))
            .build();
        let updated = apply(
            &FileDirectory,
            &c,
            &spec(json!({"path": "/srv/app", "mode": "0700"})),
        )
        .await;
        assert!(updated.artefacts.contains(&Artefact::Directory {
            path: "/srv/app".into()
        }));
        let rec = record("file.directory", "/srv/app", &updated);
        assert!(FileDirectory.remove(&cx(&sys), &rec).await.unwrap().changed);
        assert!(!sys.exists("/srv/app"));
    }
}
