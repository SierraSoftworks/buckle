//! `file.symlink`: ensure a symbolic link points at a target.

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FileKind;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Ensures `path` is a symbolic link to `target`.
///
/// ```yaml
/// - file.symlink:
///     path: /usr/local/bin/nomad
///     target: /opt/nomad/1.9.0/nomad
///     force: true       # replace an existing file (or empty directory) at `path`
/// ```
///
/// A symlink pointing elsewhere is always replaced. Anything else at `path`
/// blocks the plan unless `force: true`; `force` deletes a file but will not
/// delete a directory that has contents. Removal deletes the link if it still
/// points where Buckle pointed it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the link.
    pub path: PathBuf,
    /// What the link points to (not required to exist).
    pub target: PathBuf,
    /// Replace a regular file or empty directory found at `path`.
    #[serde(default)]
    pub force: bool,
}

pub struct FileSymlink;
buckle_core::register_task!(FileSymlink);

fn target_digest(target: &std::path::Path) -> Digest {
    Digest::of(target.to_string_lossy().as_bytes())
}

enum Found {
    Missing,
    Correct,
    WrongLink(PathBuf),
    Other(FileKind),
}

impl FileSymlink {
    async fn find(&self, cx: &Cx, spec: &Spec) -> Result<Found> {
        hostkit::require_absolute(&spec.path, "path")?;
        let Some(meta) = cx.system.stat(&spec.path).await? else {
            return Ok(Found::Missing);
        };
        if meta.kind != FileKind::Symlink {
            return Ok(Found::Other(meta.kind));
        }
        Ok(match cx.system.read_link(&spec.path).await? {
            Some(t) if t == spec.target => Found::Correct,
            Some(t) => Found::WrongLink(t),
            None => Found::Other(FileKind::Other),
        })
    }
}

#[async_trait]
impl Task for FileSymlink {
    type Spec = Spec;
    const KIND: &'static str = "file.symlink";
    const PLATFORMS: Platforms = Platforms::UNIX;
    const DESCRIPTION: &'static str =
        "Ensure a symbolic link exists and points at the given target.";

    fn key(spec: &Spec) -> String {
        spec.path.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        format!("{} → {}", spec.path.display(), spec.target.display())
    }

    #[instrument(name = "task.file.symlink.plan", skip_all, fields(path = %spec.path.display(), target = %spec.target.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let path = spec.path.display();
        Ok(match self.find(cx, spec).await? {
            Found::Correct => Planned::NoOp,
            Found::Missing => Planned::change(Diff::Create {
                summary: format!("link {path} → {}", spec.target.display()),
                preview: None,
            }),
            Found::WrongLink(old) => Planned::change(Diff::Update {
                summary: format!(
                    "repoint {path}: {} → {}",
                    old.display(),
                    spec.target.display()
                ),
                unified: None,
            }),
            Found::Other(_) if spec.force => Planned::change(Diff::Update {
                summary: format!("replace {path} with a link to {}", spec.target.display()),
                unified: None,
            }),
            Found::Other(kind) => Planned::blocked(format!(
                "{path} already exists as a {kind:?}; set `force: true` to replace it with a symlink"
            )),
        })
    }

    #[instrument(name = "task.file.symlink.apply", skip_all, fields(path = %spec.path.display(), target = %spec.target.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let applied = |summary: String, changed: bool| {
            let mut a = if changed {
                Applied::changed(summary)
            } else {
                Applied::unchanged(summary)
            };
            a.artefacts.push(Artefact::Symlink {
                path: spec.path.clone(),
            });
            a.content_digest = Some(target_digest(&spec.target));
            a
        };
        match self.find(cx, spec).await? {
            Found::Correct => Ok(applied(
                format!("{} already links correctly", spec.path.display()),
                false,
            )),
            Found::Missing => {
                cx.system.symlink(&spec.target, &spec.path).await?;
                Ok(applied(format!("linked {}", spec.path.display()), true))
            }
            Found::WrongLink(_) => {
                cx.system.remove(&spec.path, false).await?;
                cx.system.symlink(&spec.target, &spec.path).await?;
                Ok(applied(format!("repointed {}", spec.path.display()), true))
            }
            Found::Other(_) if spec.force => {
                cx.system.remove(&spec.path, false).await.map_err(|e| {
                    error::wrap_user(
                        e,
                        format!("Could not replace {} with a symlink.", spec.path.display()),
                        &["`force` replaces files and empty directories; empty or move the directory first."],
                    )
                })?;
                cx.system.symlink(&spec.target, &spec.path).await?;
                Ok(applied(
                    format!("replaced {} with a link", spec.path.display()),
                    true,
                ))
            }
            Found::Other(kind) => Err(error::user(
                format!("{} already exists as a {kind:?}.", spec.path.display()),
                &["Set `force: true` to replace it, or choose another `path`."],
            )),
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!("delete the symlink {} if it is unchanged", record.key.key)
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(path) = record.artefacts.iter().find_map(|a| match a {
            Artefact::Symlink { path } => Some(path.clone()),
            _ => None,
        }) else {
            return Ok(Applied::skipped("nothing recorded to remove"));
        };
        match cx.system.read_link(&path).await? {
            None => Ok(Applied::unchanged(format!(
                "{} is no longer a symlink, left in place",
                path.display()
            ))),
            Some(target)
                if record
                    .content_digest
                    .as_ref()
                    .is_none_or(|d| *d == target_digest(&target)) =>
            {
                cx.system.remove(&path, false).await?;
                Ok(Applied::changed(format!(
                    "deleted symlink {}",
                    path.display()
                )))
            }
            Some(_) => Ok(Applied::skipped(format!(
                "{} changed outside Buckle, left in place",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::{FilePerms, System};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;
    use std::path::Path;

    async fn linked(target: &str) -> FakeSystem {
        let sys = FakeSystem::new();
        sys.symlink(Path::new(target), Path::new("/usr/bin/tool"))
            .await
            .unwrap();
        sys
    }

    #[rstest]
    #[case::missing(FakeSystem::new(), false, Class::Change)]
    #[case::file_in_the_way(FakeSystem::new().with_file("/usr/bin/tool", "x", FilePerms::default()), false, Class::Blocked)]
    #[case::file_replaced_with_force(FakeSystem::new().with_file("/usr/bin/tool", "x", FilePerms::default()), true, Class::Change)]
    #[tokio::test]
    async fn plan_outcomes(#[case] sys: FakeSystem, #[case] force: bool, #[case] expected: Class) {
        let s: Spec =
            spec(json!({"path": "/usr/bin/tool", "target": "/opt/tool/v1", "force": force}));
        assert_eq!(plan_class(&FileSymlink, &cx(&sys), &s).await, expected);
    }

    #[tokio::test]
    async fn correct_links_are_noops_and_wrong_ones_are_repointed() {
        let s: Spec = spec(json!({"path": "/usr/bin/tool", "target": "/opt/tool/v2"}));
        let sys = linked("/opt/tool/v2").await;
        assert_eq!(plan_class(&FileSymlink, &cx(&sys), &s).await, Class::NoOp);

        let sys = linked("/opt/tool/v1").await;
        let c = cx(&sys);
        let planned = FileSymlink.plan(&c, &s).await.unwrap();
        assert!(
            diff(&planned)
                .summary()
                .contains("/opt/tool/v1 → /opt/tool/v2")
        );
        FileSymlink.apply(&c, &s, &planned).await.unwrap();
        assert_eq!(
            sys.read_link(Path::new("/usr/bin/tool"))
                .await
                .unwrap()
                .unwrap(),
            Path::new("/opt/tool/v2")
        );
        assert_eq!(plan_class(&FileSymlink, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn force_replaces_files_but_not_populated_directories() {
        let sys = FakeSystem::new().with_file("/usr/bin/tool", "x", FilePerms::default());
        let s: Spec = spec(json!({"path": "/usr/bin/tool", "target": "/opt/t", "force": true}));
        let c = cx(&sys);
        apply(&FileSymlink, &c, &s).await;
        assert!(
            sys.read_link(Path::new("/usr/bin/tool"))
                .await
                .unwrap()
                .is_some()
        );

        let sys = FakeSystem::new().with_file("/usr/bin/tool/inner", "x", FilePerms::default());
        let c = cx(&sys);
        assert!(FileSymlink.apply(&c, &s, &Planned::NoOp).await.is_err());
        assert!(sys.exists("/usr/bin/tool/inner"));
    }

    #[tokio::test]
    async fn removal_deletes_links_that_still_point_where_we_put_them() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let s: Spec = spec(json!({"path": "/usr/bin/tool", "target": "/opt/t"}));
        let rec = record(
            "file.symlink",
            "/usr/bin/tool",
            &apply(&FileSymlink, &c, &s).await,
        );

        sys.remove(Path::new("/usr/bin/tool"), false).await.unwrap();
        sys.symlink(Path::new("/elsewhere"), Path::new("/usr/bin/tool"))
            .await
            .unwrap();
        let out = FileSymlink.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("changed outside Buckle"));
        assert!(sys.exists("/usr/bin/tool"));

        sys.remove(Path::new("/usr/bin/tool"), false).await.unwrap();
        sys.symlink(Path::new("/opt/t"), Path::new("/usr/bin/tool"))
            .await
            .unwrap();
        assert!(FileSymlink.remove(&c, &rec).await.unwrap().changed);
        assert!(!sys.exists("/usr/bin/tool"));
    }
}
