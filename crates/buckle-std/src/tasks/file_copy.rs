//! `file.copy`: copy a file from the module's `files/` directory to the host.

use super::hostkit::{self, describe_perms, diffable_text, digest_change, effective_perms};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Copies a file from the module's `files/` directory to `dest`.
///
/// ```yaml
/// - file.copy:
///     src: nomad/client.hcl        # relative to the module's files/ directory
///     dest: /etc/nomad.d/client.hcl
///     mode: "0640"                  # optional; owner and group too
///     owner: root
///     group: nomad
///     backup: true                  # keep a timestamped copy of the file being replaced
/// ```
///
/// The file is written atomically. Plans show a unified diff for text files
/// and a digest change for binary ones. Removal deletes `dest` only if its
/// content still matches what Buckle wrote; otherwise it is left in place.
///
/// `validate` is a command (argv) that checks the new content before it
/// replaces the file (and while planning); `{path}` stands for a private
/// temporary copy under `<state_dir>/tmp`, deleted afterwards. A non-zero
/// exit fails the task and leaves the file untouched. Without `validate`,
/// `/etc/sudoers`, `/etc/sudoers.d/*` and `sshd_config` files are checked with
/// `visudo -cf {path}` and `sshd -t -f {path}` (when those tools are
/// installed); `validate: []` turns that off.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Source path, relative to the module's `files/` directory.
    pub src: String,
    /// Absolute destination path on the host.
    pub dest: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Keep a timestamped `.bak` copy of the destination before replacing its content.
    #[serde(default)]
    pub backup: bool,
    /// A command that checks the new content; `{path}` stands for the file to check.
    /// Default: automatic for sudoers and sshd_config; `[]` disables.
    #[serde(default)]
    pub validate: Option<Vec<String>>,
}

pub struct FileCopy;
buckle_core::register_task!(FileCopy);

struct Inspection {
    planned: Planned,
    content: Vec<u8>,
    existing: Option<(FileMeta, Vec<u8>)>,
}

impl FileCopy {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        let content = hostkit::read_module_file(cx, &spec.src).await?;
        let dest = spec.dest.display();
        let Some(meta) = cx.system.stat(&spec.dest).await? else {
            return Ok(Inspection {
                planned: Planned::change(Diff::Create {
                    summary: format!("create {dest} from {}", spec.src),
                    preview: Some(hostkit::text_preview(&content)),
                }),
                content,
                existing: None,
            });
        };
        if meta.kind == FileKind::Directory {
            return Err(error::user(
                format!("{dest} is a directory, so file.copy cannot write a file there."),
                &["Point `dest` at the file path, not its directory."],
            ));
        }
        let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
        let same = Digest::of(&current) == Digest::of(&content);
        let perms = hostkit::perm_changes(&meta, &spec.perms);
        let planned = if same && perms.is_empty() {
            Planned::NoOp
        } else if same {
            Planned::change(Diff::Update {
                summary: format!("{dest}: {}", perms.join(", ")),
                unified: None,
            })
        } else {
            let mut summary = format!("update {dest}");
            if !perms.is_empty() {
                summary.push_str(&format!(" (and {})", perms.join(", ")));
            }
            match (diffable_text(&current), diffable_text(&content)) {
                // The host's current content goes through the same rules as
                // other editors: scrubbed, no context for sensitive files, and
                // not shown at all when the file has held secrets.
                (Some(old), Some(new)) => {
                    Planned::change(hostkit::text_diff(cx, &spec.dest, summary, old, new))
                }
                _ => Planned::change(Diff::Update {
                    summary: format!(
                        "{summary}: content changed ({})",
                        digest_change(&Digest::of(&current), &Digest::of(&content))
                    ),
                    unified: None,
                }),
            }
        };
        Ok(Inspection {
            planned,
            content,
            existing: Some((meta, current)),
        })
    }
}

#[async_trait]
impl Task for FileCopy {
    type Spec = Spec;
    const KIND: &'static str = "file.copy";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Copy a file from the module's files/ directory to the host, with mode, owner and group.";

    fn key(spec: &Spec) -> String {
        spec.dest.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        if perms.is_empty() {
            spec.dest.display().to_string()
        } else {
            format!("{} ({perms})", spec.dest.display())
        }
    }

    #[instrument(name = "task.file.copy.plan", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let inspection = self.inspect(cx, spec).await?;
        if inspection.planned.is_change()
            && content_differs(&inspection)
            && let hostkit::validate::Checked::ToolMissing(reason) = hostkit::validate::check(
                cx,
                &spec.dest,
                spec.validate.as_deref(),
                hostkit::validate::Content::Bytes(&inspection.content),
            )
            .await?
        {
            return Ok(Planned::blocked(reason));
        }
        Ok(inspection.planned)
    }

    #[instrument(name = "task.file.copy.apply", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let inspection = self.inspect(cx, spec).await?;
        let digest = Digest::of(&inspection.content);
        let shadow = hostkit::shadow_of(&inspection.content);
        let dest = &spec.dest;
        if !inspection.planned.is_change() {
            return Ok(hostkit::ownership::file_written(
                cx,
                format!("{} already up to date", dest.display()),
                dest,
                false,
                digest,
                shadow,
                false,
            ));
        }
        let content_differs = content_differs(&inspection);
        if content_differs {
            hostkit::validate::check(
                cx,
                dest,
                spec.validate.as_deref(),
                hostkit::validate::Content::Bytes(&inspection.content),
            )
            .await?;
        }
        match &inspection.existing {
            Some((meta, current)) if content_differs => {
                if spec.backup {
                    hostkit::backup_file(cx, dest, meta, current).await?;
                }
                let perms = effective_perms(&spec.perms, Some(meta));
                cx.system
                    .write_atomic(dest, &inspection.content, &perms)
                    .await?;
            }
            Some(_) => cx.system.set_perms(dest, &spec.perms).await?,
            None => {
                cx.system
                    .write_atomic(dest, &inspection.content, &spec.perms)
                    .await?;
            }
        }
        // A permission change on a file Buckle never wrote adopts it rather than claiming it.
        Ok(hostkit::ownership::file_written(
            cx,
            if content_differs {
                format!("wrote {}", dest.display())
            } else {
                format!("updated permissions of {}", dest.display())
            },
            dest,
            content_differs,
            digest,
            shadow,
            true,
        ))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "delete {} if it is unchanged since Buckle wrote it",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        hostkit::remove_recorded_file(cx, record).await
    }
}

/// Whether applying rewrites the content (as opposed to only permissions).
fn content_differs(inspection: &Inspection) -> bool {
    inspection
        .existing
        .as_ref()
        .is_none_or(|(_, current)| Digest::of(current) != Digest::of(&inspection.content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::{FakeSystem, MemorySource};
    use rstest::rstest;
    use serde_json::json;

    fn source() -> MemorySource {
        MemorySource::new()
            .with("app/app.conf", "listen = 80\n")
            .with("app/blob.bin", [0u8, 159, 146, 150])
    }

    fn base(sys: FakeSystem) -> FakeSystem {
        sys.with_dir("/etc/app", FilePerms::mode(0o755))
    }

    #[rstest]
    #[case::missing_dest(base(FakeSystem::new()), json!({}), Class::Change)]
    #[case::identical(
        base(FakeSystem::new()).with_file("/etc/app/app.conf", "listen = 80\n", FilePerms::mode(0o644)),
        json!({}),
        Class::NoOp
    )]
    #[case::content_differs(
        base(FakeSystem::new()).with_file("/etc/app/app.conf", "listen = 8080\n", FilePerms::mode(0o644)),
        json!({}),
        Class::Change
    )]
    #[case::mode_differs(
        base(FakeSystem::new()).with_file("/etc/app/app.conf", "listen = 80\n", FilePerms::mode(0o600)),
        json!({"mode": "0644"}),
        Class::Change
    )]
    #[case::mode_matches(
        base(FakeSystem::new()).with_file("/etc/app/app.conf", "listen = 80\n", FilePerms::mode(0o600)),
        json!({"mode": "0600"}),
        Class::NoOp
    )]
    #[case::unspecified_mode_is_not_compared(
        base(FakeSystem::new()).with_file("/etc/app/app.conf", "listen = 80\n", FilePerms::mode(0o600)),
        json!({}),
        Class::NoOp
    )]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        let mut raw = json!({"src": "app/app.conf", "dest": "/etc/app/app.conf"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let s: Spec = spec(raw);
        let cx = cx_plan(&sys, source());
        assert_eq!(plan_class(&FileCopy, &cx, &s).await, expected);
    }

    #[tokio::test]
    async fn create_previews_the_source_text_and_apply_writes_it() {
        let sys = FakeSystem::new();
        let s: Spec = spec(
            json!({"src": "app/app.conf", "dest": "/etc/app/app.conf", "mode": "0640", "owner": "root"}),
        );
        let cx = cx_src(&sys, source());
        let planned = FileCopy.plan(&cx, &s).await.unwrap();
        let Diff::Create {
            preview: Some(p), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert_eq!(p.get(), "listen = 80\n");

        let applied = FileCopy.apply(&cx, &s, &planned).await.unwrap();
        assert!(applied.changed);
        assert_eq!(
            sys.file_string("/etc/app/app.conf").unwrap(),
            "listen = 80\n"
        );
        let meta =
            buckle_core::system::System::stat(&sys, std::path::Path::new("/etc/app/app.conf"))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(meta.mode, Some(0o640));
        assert_eq!(applied.content_digest, Some(Digest::of(b"listen = 80\n")));
        assert!(applied.shadow.is_some());
    }

    #[tokio::test]
    async fn binary_content_previews_as_binary_and_diffs_by_digest() {
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "app/blob.bin", "dest": "/opt/blob.bin"}));
        let cx = cx_src(&sys, source());
        let planned = FileCopy.plan(&cx, &s).await.unwrap();
        let Diff::Create {
            preview: Some(p), ..
        } = diff(&planned)
        else {
            panic!()
        };
        assert_eq!(p.get(), "binary");

        let sys = FakeSystem::new().with_file("/opt/blob.bin", [1u8, 2, 200], FilePerms::default());
        let cx = cx_src(&sys, source());
        let planned = FileCopy.plan(&cx, &s).await.unwrap();
        let Diff::Update {
            summary,
            unified: None,
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(summary.contains("digest"), "{summary}");
    }

    #[tokio::test]
    async fn update_shows_a_unified_diff() {
        let sys = FakeSystem::new().with_file(
            "/etc/app/app.conf",
            "listen = 8080\n",
            FilePerms::default(),
        );
        let s: Spec = spec(json!({"src": "app/app.conf", "dest": "/etc/app/app.conf"}));
        let planned = FileCopy.plan(&cx_src(&sys, source()), &s).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!()
        };
        assert!(u.get().contains("-listen = 8080"));
        assert!(u.get().contains("+listen = 80"));
    }

    #[tokio::test]
    async fn applying_twice_is_idempotent() {
        let sys = FakeSystem::new();
        let s: Spec =
            spec(json!({"src": "app/app.conf", "dest": "/etc/app/app.conf", "mode": "0600"}));
        let cx = cx_src(&sys, source());
        apply(&FileCopy, &cx, &s).await;
        assert_eq!(plan_class(&FileCopy, &cx, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn perms_only_changes_leave_content_alone() {
        let sys = FakeSystem::new().with_file(
            "/etc/app/app.conf",
            "listen = 80\n",
            FilePerms::mode(0o600),
        );
        let s: Spec =
            spec(json!({"src": "app/app.conf", "dest": "/etc/app/app.conf", "mode": "0644"}));
        let cx = cx_src(&sys, source());
        let applied = apply(&FileCopy, &cx, &s).await;
        assert!(applied.changed);
        assert_eq!(plan_class(&FileCopy, &cx, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn backup_keeps_the_replaced_content() {
        let sys = FakeSystem::new().with_file("/etc/app/app.conf", "old\n", FilePerms::default());
        let s: Spec =
            spec(json!({"src": "app/app.conf", "dest": "/etc/app/app.conf", "backup": true}));
        apply(&FileCopy, &cx_src(&sys, source()), &s).await;
        let backups: Vec<_> = sys
            .paths()
            .into_iter()
            .filter(|p| p.to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(sys.file_string(&backups[0]).unwrap(), "old\n");
        assert_eq!(
            sys.file_string("/etc/app/app.conf").unwrap(),
            "listen = 80\n"
        );
    }

    #[tokio::test]
    async fn missing_source_and_relative_dest_are_user_errors() {
        let sys = FakeSystem::new();
        let cx = cx_src(&sys, source());
        let s: Spec = spec(json!({"src": "nope", "dest": "/etc/x"}));
        let err = FileCopy.plan(&cx, &s).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        let s: Spec = spec(json!({"src": "app/app.conf", "dest": "etc/x"}));
        assert!(FileCopy.plan(&cx, &s).await.is_err());
        let sys = FakeSystem::new().with_dir("/etc/x", FilePerms::default());
        let s: Spec = spec(json!({"src": "app/app.conf", "dest": "/etc/x"}));
        assert!(FileCopy.plan(&cx_src(&sys, source()), &s).await.is_err());
    }

    #[tokio::test]
    async fn removal_deletes_unchanged_files_and_leaves_drifted_ones() {
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "app/app.conf", "dest": "/etc/app/app.conf"}));
        let cx = cx_src(&sys, source());
        let applied = apply(&FileCopy, &cx, &s).await;
        let rec = record("file.copy", "/etc/app/app.conf", &applied);

        // Drifted: hand-edited after Buckle wrote it.
        put(&sys, "/etc/app/app.conf", "hand edit\n").await;
        let out = FileCopy.remove(&cx, &rec).await.unwrap();
        assert!(!out.changed);
        assert!(
            out.summary
                .contains("changed outside Buckle, left in place"),
            "{}",
            out.summary
        );
        assert!(sys.exists("/etc/app/app.conf"));

        // Restored: removal now deletes it.
        put(&sys, "/etc/app/app.conf", "listen = 80\n").await;
        let out = FileCopy.remove(&cx, &rec).await.unwrap();
        assert!(out.changed);
        assert!(!sys.exists("/etc/app/app.conf"));

        // Already gone: nothing to do.
        assert!(!FileCopy.remove(&cx, &rec).await.unwrap().changed);
    }

    #[test]
    fn spec_flattens_perms_and_defaults_the_rest() {
        let s: Spec = spec(
            json!({"src": "a", "dest": "/b", "mode": "0755", "owner": "root", "group": "wheel"}),
        );
        assert_eq!(s.perms.mode, Some(0o755));
        assert_eq!(s.perms.group.as_deref(), Some("wheel"));
        assert!(!s.backup);
    }

    fn sudoers_source() -> MemorySource {
        MemorySource::new().with("sudoers/deploy", "deploy ALL=(ALL) NOPASSWD: ALL\n")
    }

    #[tokio::test]
    async fn sudoers_files_are_checked_with_visudo_in_plan_and_apply() {
        let sys = FakeSystem::new()
            .with_dir("/etc/sudoers.d", FilePerms::mode(0o750))
            .with_env("BUCKLE_STATE_DIR", "/srv/buckle-state")
            .on_command_fail("visudo", &["-cf"], 1, ">>> parse error near line 1 <<<");
        let c = cx_src(&sys, sudoers_source());
        let s: Spec =
            spec(json!({"src": "sudoers/deploy", "dest": "/etc/sudoers.d/deploy", "mode": "0440"}));
        let err = FileCopy.plan(&c, &s).await.unwrap_err();
        assert!(
            err.description().contains("parse error"),
            "{}",
            err.description()
        );
        assert!(FileCopy.apply(&c, &s, &Planned::NoOp).await.is_err());
        assert!(
            !sys.exists("/etc/sudoers.d/deploy"),
            "nothing was installed"
        );
        let ran = sys.commands_run();
        let checked = ran.iter().find(|c| c.program == "visudo").unwrap();
        assert!(
            checked.args[1]
                .replace('\\', "/")
                .starts_with("/srv/buckle-state/tmp/"),
            "{:?}",
            checked.args
        );
        assert!(
            sys.paths()
                .iter()
                .all(|p| !p.starts_with("/srv/buckle-state/tmp/validate-"))
        );
        assert!(
            sys.paths()
                .iter()
                .all(|p| !p.to_string_lossy().starts_with("/etc/sudoers.d/.")),
            "no temp next to dest"
        );

        // `validate: []` turns the automatic check off.
        let off: Spec =
            spec(json!({"src": "sudoers/deploy", "dest": "/etc/sudoers.d/deploy", "validate": []}));
        apply(&FileCopy, &c, &off).await;
        assert!(sys.exists("/etc/sudoers.d/deploy"));
    }

    #[tokio::test]
    async fn a_missing_automatic_validator_is_skipped_but_an_explicit_one_blocks_the_plan() {
        let sys = FakeSystem::new().with_dir("/etc/sudoers.d", FilePerms::mode(0o750));
        let planning = cx_plan(&sys, sudoers_source());
        let auto: Spec = spec(json!({"src": "sudoers/deploy", "dest": "/etc/sudoers.d/deploy"}));
        assert_eq!(plan_class(&FileCopy, &planning, &auto).await, Class::Change);
        let explicit: Spec = spec(
            json!({"src": "sudoers/deploy", "dest": "/etc/sudoers.d/deploy", "validate": ["visudo", "-cf", "{path}"]}),
        );
        // Planning is Blocked (an earlier step may install the tool); applying
        // without it is an error, and nothing is written.
        assert_eq!(
            plan_class(&FileCopy, &planning, &explicit).await,
            Class::Blocked
        );
        let applying = cx_src(&sys, sudoers_source());
        assert!(
            FileCopy
                .apply(&applying, &explicit, &Planned::NoOp)
                .await
                .is_err()
        );
        assert!(!sys.exists("/etc/sudoers.d/deploy"));
    }
}
