//! `file.template`: render a Jinja template from the module and write it to the host.

use super::hostkit::{self, describe_perms, diffable_text, digest_change};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::FileSink;
use buckle_core::secrets::{Redacted, Rendered};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Renders a template from the module's `files/` directory and writes the result to `dest`.
///
/// ```yaml
/// - file.template:
///     src: nomad/server.hcl.j2
///     dest: /etc/nomad.d/server.hcl
///     mode: "0640"
///     owner: root
///     group: nomad
///     validate: ["nomad", "config", "validate", "{path}"]   # optional
///     backup: false
/// ```
///
/// Templates are rendered twice (design section 06): a redacted render, in
/// which secrets appear as `«secret:NAME@fingerprint»` tokens, is used for plan
/// previews and the state shadow; the real render is written through a file
/// sink and only its digest is kept. A plan therefore never contains a secret
/// value, and a rotated secret shows up as a fingerprint change on exactly the
/// lines that use it. When a file that contains secrets has drifted on disk,
/// the plan reports `content changed (digest a → b)` rather than a diff.
///
/// `validate` is a command (argv) run against the rendered content before it
/// replaces the destination (and while planning); `{path}` is replaced with a
/// private temporary file under `<state_dir>/tmp`, deleted afterwards. A
/// non-zero exit fails the task and leaves `dest` untouched. When the file
/// contains secrets, the validator's output is not echoed. Without
/// `validate`, `/etc/sudoers`, `/etc/sudoers.d/*` and `sshd_config` files are
/// checked with `visudo -cf {path}` and `sshd -t -f {path}` (when those tools
/// are installed); `validate: []` turns that off.
///
/// Removal deletes `dest` only if its content still matches what Buckle wrote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Template path, relative to the module's `files/` directory.
    pub src: String,
    /// Absolute destination path on the host.
    pub dest: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// A command that checks the rendered file; `{path}` stands for the file to check.
    /// Default: automatic for sudoers and sshd_config; `[]` disables.
    #[serde(default)]
    pub validate: Option<Vec<String>>,
    /// Keep a timestamped `.bak` copy of the destination before replacing its content.
    #[serde(default)]
    pub backup: bool,
}

pub struct FileTemplate;
buckle_core::register_task!(FileTemplate);

struct Inspection {
    planned: Planned,
    rendered: Rendered,
    redacted: Redacted<String>,
    existing: Option<(FileMeta, Vec<u8>)>,
}

impl FileTemplate {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        let text = hostkit::read_module_text(cx, &spec.src).await?;
        let rendered = cx.templater.render_sensitive(&text, &cx.host)?;
        let redacted = cx.templater.render_redacted(&text, &cx.host)?;
        let new_digest = rendered.digest();
        let dest = spec.dest.display();

        let Some(meta) = cx.system.stat(&spec.dest).await? else {
            return Ok(Inspection {
                planned: Planned::change(Diff::Create {
                    summary: format!("create {dest} from {}", spec.src),
                    preview: Some(hostkit::truncate_redacted(redacted.clone())),
                }),
                rendered,
                redacted,
                existing: None,
            });
        };
        if meta.kind == FileKind::Directory {
            return Err(error::user(
                format!("{dest} is a directory, so file.template cannot write a file there."),
                &["Point `dest` at the file path, not its directory."],
            ));
        }
        let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
        let current_digest = Digest::of(&current);
        let perms = hostkit::perm_changes(&meta, &spec.perms);
        let planned = if current_digest == new_digest {
            if perms.is_empty() {
                Planned::NoOp
            } else {
                Planned::change(Diff::Update {
                    summary: format!("{dest}: {}", perms.join(", ")),
                    unified: None,
                })
            }
        } else {
            let mut summary = format!("update {dest}");
            if !perms.is_empty() {
                summary.push_str(&format!(" (and {})", perms.join(", ")));
            }
            // The file is known to hold secrets when the new render does, or
            // when the shadow recorded last time does. Such a file is never read
            // back into a plan: the old side is the previous redacted shadow (a
            // rotated secret shows as a fingerprint change on exactly its line),
            // provided the file is still what Buckle wrote; otherwise only the
            // digest change is shown.
            let previous = cx.previous.as_deref();
            let sensitive = rendered.is_sensitive() || hostkit::ever_sensitive(cx);
            let unchanged_since_written = previous
                .and_then(|r| r.content_digest.as_ref())
                .is_some_and(|d| *d == current_digest);
            let context = if sensitive {
                0
            } else {
                hostkit::diff_context(cx, &spec.dest)
            };
            match (
                sensitive,
                previous.and_then(|r| r.shadow.as_ref()),
                diffable_text(&current),
            ) {
                (true, Some(shadow), _) if unchanged_since_written => Planned::change(
                    hostkit::redacted_unified(cx, summary, shadow.get(), redacted.get(), context),
                ),
                (false, _, Some(old)) => Planned::change(hostkit::redacted_unified(
                    cx,
                    summary,
                    old,
                    redacted.get(),
                    context,
                )),
                _ => {
                    let preview = hostkit::truncate_redacted(redacted.clone())
                        .map(|p| format!("new content (secrets redacted):\n{p}"));
                    Planned::change(Diff::Update {
                        summary: format!(
                            "{summary}: content changed ({})",
                            digest_change(&current_digest, &new_digest)
                        ),
                        unified: Some(preview),
                    })
                }
            }
        };
        Ok(Inspection {
            planned,
            rendered,
            redacted,
            existing: Some((meta, current)),
        })
    }
}

#[async_trait]
impl Task for FileTemplate {
    type Spec = Spec;
    const KIND: &'static str = "file.template";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Render a template from the module's files/ directory (secret-aware) and write it to the host.";

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

    #[instrument(name = "task.file.template.plan", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let inspection = self.inspect(cx, spec).await?;
        if inspection.planned.is_change() && content_changes(&inspection) {
            hostkit::validate::check(
                cx,
                &spec.dest,
                spec.validate.as_deref(),
                hostkit::validate::Content::Rendered(&inspection.rendered),
            )
            .await?;
        }
        Ok(inspection.planned)
    }

    #[instrument(name = "task.file.template.apply", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let inspection = self.inspect(cx, spec).await?;
        let digest = inspection.rendered.digest();
        let dest = &spec.dest;
        let shadow = (inspection.redacted.get().len() <= hostkit::SHADOW_LIMIT)
            .then(|| inspection.redacted.clone());
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
        let wrote_content = content_changes(&inspection);
        if wrote_content {
            hostkit::validate::check(
                cx,
                &spec.dest,
                spec.validate.as_deref(),
                hostkit::validate::Content::Rendered(&inspection.rendered),
            )
            .await?;
            let existing_meta = inspection.existing.as_ref().map(|(m, _)| m);
            if spec.backup
                && let Some((meta, current)) = &inspection.existing
            {
                hostkit::backup_file(cx, dest, meta, current).await?;
            }
            // A new file with secrets in it is not world-readable unless asked.
            let perms = hostkit::effective_perms_sensitive(
                &spec.perms,
                existing_meta,
                inspection.rendered.is_sensitive(),
            );
            FileSink
                .write_rendered(&*cx.system, dest, &inspection.rendered, &perms)
                .await?;
        } else {
            cx.system.set_perms(dest, &spec.perms).await?;
        }
        // A permission change on a file Buckle never wrote adopts it rather than
        // claiming it, so that removal does not delete a file Buckle did not create.
        Ok(hostkit::ownership::file_written(
            cx,
            if wrote_content {
                format!("wrote {}", dest.display())
            } else {
                format!("updated permissions of {}", dest.display())
            },
            dest,
            wrote_content,
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

/// Whether applying the planned change rewrites the content (as opposed to only permissions).
fn content_changes(inspection: &Inspection) -> bool {
    match &inspection.existing {
        None => true,
        Some((_, current)) => Digest::of(current) != inspection.rendered.digest(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::{FakeSystem, MemorySource, ok_output};
    use rstest::rstest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn vars_cx(sys: &FakeSystem, source: MemorySource) -> Cx {
        use buckle_core::selectors::{HostContext, HostIdentity};
        let mut vars = buckle_core::selectors::Vars::new();
        vars.insert("name".into(), json!("nomad"));
        let host = HostContext::new(HostIdentity {
            id: "h".into(),
            ..Default::default()
        })
        .with_vars(vars);
        buckle_core::testing::CxBuilder::new(sys.clone())
            .source(source)
            .host(host)
            .build()
    }

    fn source() -> MemorySource {
        MemorySource::new()
            .with("app.conf.j2", "name = {{ vars.name }}\n")
            .with(
                "secret.conf.j2",
                "name = {{ vars.name }}\ntoken = {{ secrets.token }}\n",
            )
    }

    #[rstest]
    #[case::missing(None, Class::Change)]
    #[case::identical(Some("name = nomad\n"), Class::NoOp)]
    #[case::different(Some("name = other\n"), Class::Change)]
    #[tokio::test]
    async fn plan_outcomes(#[case] current: Option<&str>, #[case] expected: Class) {
        let mut sys = FakeSystem::new();
        if let Some(c) = current {
            sys = sys.with_file("/etc/app.conf", c, FilePerms::mode(0o644));
        }
        let s: Spec = spec(json!({"src": "app.conf.j2", "dest": "/etc/app.conf"}));
        assert_eq!(
            plan_class(&FileTemplate, &vars_cx(&sys, source()), &s).await,
            expected
        );
    }

    #[tokio::test]
    async fn plain_templates_show_a_real_diff_and_are_idempotent() {
        let sys =
            FakeSystem::new().with_file("/etc/app.conf", "name = other\n", FilePerms::default());
        let s: Spec = spec(json!({"src": "app.conf.j2", "dest": "/etc/app.conf", "mode": "0600"}));
        let cx = vars_cx(&sys, source());
        let planned = FileTemplate.plan(&cx, &s).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(u.get().contains("-name = other") && u.get().contains("+name = nomad"));

        let applied = FileTemplate.apply(&cx, &s, &planned).await.unwrap();
        assert!(applied.changed);
        assert_eq!(sys.file_string("/etc/app.conf").unwrap(), "name = nomad\n");
        assert_eq!(applied.shadow.as_ref().unwrap().get(), "name = nomad\n");
        assert_eq!(plan_class(&FileTemplate, &cx, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn secrets_are_written_but_never_appear_in_plans_diffs_or_results() {
        const SECRET: &str = "hunter2-very-secret";
        let sys = FakeSystem::new();
        let s: Spec =
            spec(json!({"src": "secret.conf.j2", "dest": "/etc/app.conf", "mode": "0600"}));
        let cx = cx_secrets(&sys, source_with_name(), secrets("token", SECRET));
        let planned = FileTemplate.plan(&cx, &s).await.unwrap();
        let applied = FileTemplate.apply(&cx, &s, &planned).await.unwrap();

        assert!(
            sys.file_string("/etc/app.conf").unwrap().contains(SECRET),
            "the real value is written"
        );
        let everything = serialised(&planned, &applied);
        assert!(!everything.contains(SECRET), "leaked: {everything}");
        assert!(
            everything.contains("secret:token@"),
            "the redacted token is shown"
        );
        assert_eq!(
            plan_class(&FileTemplate, &cx, &s).await,
            Class::NoOp,
            "idempotent"
        );

        // The secret rotates: the plan still must not contain either value.
        let rotated = cx_secrets(
            &sys,
            source_with_name(),
            secrets("token", "rotated-value-xyz"),
        );
        let planned = FileTemplate.plan(&rotated, &s).await.unwrap();
        let rendered = serde_json::to_string(&planned).unwrap();
        assert!(
            !rendered.contains(SECRET) && !rendered.contains("rotated-value-xyz"),
            "{rendered}"
        );
        let Diff::Update {
            summary,
            unified: Some(u),
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(summary.contains("content changed (digest"), "{summary}");
        assert!(u.get().contains("secret:token@"));
        assert!(
            !u.get().contains("name = other"),
            "the drifted on-disk text is never read back into a plan"
        );
    }

    fn source_with_name() -> MemorySource {
        MemorySource::new().with("secret.conf.j2", "token = {{ secrets.token }}\n")
    }

    #[tokio::test]
    async fn drifted_secret_files_report_a_digest_change_not_their_content() {
        let sys = FakeSystem::new().with_file(
            "/etc/app.conf",
            "token = hand-edited-value\n",
            FilePerms::default(),
        );
        let s: Spec = spec(json!({"src": "secret.conf.j2", "dest": "/etc/app.conf"}));
        let cx = cx_secrets(&sys, source_with_name(), secrets("token", "abc123"));
        let planned = FileTemplate.plan(&cx, &s).await.unwrap();
        let json = serde_json::to_string(&planned).unwrap();
        assert!(
            !json.contains("hand-edited-value") && !json.contains("abc123"),
            "{json}"
        );
        assert!(json.contains("digest"));
    }

    #[tokio::test]
    async fn validate_runs_against_the_real_render_and_cleans_up() {
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let sys = FakeSystem::new();
        let probe = sys.clone();
        let sink = seen.clone();
        let sys = sys.on_command("checker", &["--check"], move |cmd| {
            let path = cmd.args.last().unwrap().clone();
            let content = probe.file_string(&path).unwrap_or_default();
            sink.lock().unwrap().push((path, content));
            ok_output("ok")
        });
        let s: Spec = spec(
            json!({"src": "app.conf.j2", "dest": "/etc/app/app.conf", "validate": ["checker", "--check", "{path}"]}),
        );
        let cx = vars_cx(&sys, source());
        apply(&FileTemplate, &cx, &s).await;
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "the validator ran (in plan and in apply)");
        for (path, content) in seen.iter() {
            assert_ne!(path, "{path}", "the placeholder was substituted");
            assert!(
                path.replace('\\', "/").starts_with("/var/lib/buckle/tmp/"),
                "the temp file is in Buckle's scratch directory, not next to dest: {path}"
            );
            assert_eq!(content, "name = nomad\n", "it saw the real render");
            assert!(!sys.exists(path), "the temp file is removed");
        }
        assert_eq!(
            sys.file_string("/etc/app/app.conf").unwrap(),
            "name = nomad\n"
        );
    }

    #[tokio::test]
    async fn a_failing_validator_blocks_the_write_and_reports_its_stderr() {
        let sys = FakeSystem::new()
            .with_file("/etc/app.conf", "name = old\n", FilePerms::default())
            .on_command_fail("checker", &[], 2, "line 1: unknown directive");
        let s: Spec = spec(
            json!({"src": "app.conf.j2", "dest": "/etc/app.conf", "validate": ["checker", "{path}"]}),
        );
        let cx = vars_cx(&sys, source());
        let err = FileTemplate.plan(&cx, &s).await.unwrap_err();
        assert!(
            err.description().contains("unknown directive"),
            "{}",
            err.description()
        );
        assert!(err.is(buckle_core::error::Kind::User));

        let err = FileTemplate
            .apply(&cx, &s, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("unknown directive"));
        assert_eq!(
            sys.file_string("/etc/app.conf").unwrap(),
            "name = old\n",
            "dest untouched"
        );
        assert!(
            sys.paths()
                .iter()
                .all(|p| !p.to_string_lossy().contains("validate-"))
        );
    }

    #[tokio::test]
    async fn validators_of_secret_files_do_not_echo_their_output() {
        let sys = FakeSystem::new().on_command_fail("checker", &[], 1, "bad value: abc123");
        let s: Spec = spec(
            json!({"src": "secret.conf.j2", "dest": "/etc/app.conf", "validate": ["checker", "{path}"]}),
        );
        let cx = cx_secrets(&sys, source_with_name(), secrets("token", "abc123"));
        let err = FileTemplate.plan(&cx, &s).await.unwrap_err();
        assert!(!err.message().contains("abc123"));
    }

    #[tokio::test]
    async fn validate_without_a_placeholder_is_a_user_error() {
        let sys = FakeSystem::new();
        let s: Spec =
            spec(json!({"src": "app.conf.j2", "dest": "/etc/app.conf", "validate": ["checker"]}));
        let err = FileTemplate
            .plan(&vars_cx(&sys, source()), &s)
            .await
            .unwrap_err();
        assert!(err.description().contains("{path}"));
    }

    #[tokio::test]
    async fn removal_respects_drift() {
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "secret.conf.j2", "dest": "/etc/app.conf"}));
        let cx = cx_secrets(&sys, source_with_name(), secrets("token", "abc123"));
        let applied = apply(&FileTemplate, &cx, &s).await;
        let rec = record("file.template", "/etc/app.conf", &applied);
        put(&sys, "/etc/app.conf", "edited").await;
        let out = FileTemplate.remove(&cx, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("changed outside Buckle"));
        put(&sys, "/etc/app.conf", "token = abc123\n").await;
        assert!(FileTemplate.remove(&cx, &rec).await.unwrap().changed);
        assert!(!sys.exists("/etc/app.conf"));
    }

    #[tokio::test]
    async fn unreferenced_secret_names_do_not_break_plain_templates() {
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "app.conf.j2", "dest": "/etc/app.conf"}));
        let cx = {
            use buckle_core::selectors::{HostContext, HostIdentity};
            let mut vars = buckle_core::selectors::Vars::new();
            vars.insert("name".into(), json!("nomad"));
            let host = HostContext::new(HostIdentity {
                id: "h".into(),
                ..Default::default()
            })
            .with_vars(vars);
            buckle_core::testing::CxBuilder::new(sys.clone())
                .source(source())
                .host(host)
                .secrets(secrets("unused", "zzz"))
                .build()
        };
        let planned = FileTemplate.plan(&cx, &s).await.unwrap();
        assert!(matches!(
            diff(&planned),
            Diff::Create {
                preview: Some(_),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_mode_only_change_adopts_a_preexisting_file_and_removal_leaves_it() {
        let sys =
            FakeSystem::new().with_file("/etc/app.conf", "name = nomad\n", FilePerms::mode(0o644));
        let c = vars_cx(&sys, source());
        let s600 =
            spec::<Spec>(json!({"src": "app.conf.j2", "dest": "/etc/app.conf", "mode": "0600"}));
        let applied = apply(&FileTemplate, &c, &s600).await;
        assert!(
            !applied
                .artefacts
                .iter()
                .any(|a| matches!(a, buckle_core::task::Artefact::File { .. })),
            "Buckle did not write the file, so it does not own it: {:?}",
            applied.artefacts
        );
        let rec = record("file.template", "/etc/app.conf", &applied);
        let out = FileTemplate.remove(&c, &rec).await.unwrap();
        assert!(
            !out.changed && out.summary.contains("existed before"),
            "{}",
            out.summary
        );
        assert!(sys.exists("/etc/app.conf"));

        // A file Buckle wrote stays its own after a later mode-only change.
        let sys = FakeSystem::new();
        let c = vars_cx(&sys, source());
        let wrote = apply(&FileTemplate, &c, &s600).await;
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .source(source())
            .host((*c.host).clone())
            .previous(record("file.template", "/etc/app.conf", &wrote))
            .build();
        let spec_0640 =
            spec::<Spec>(json!({"src": "app.conf.j2", "dest": "/etc/app.conf", "mode": "0640"}));
        let again = apply(&FileTemplate, &c, &spec_0640).await;
        assert!(
            again
                .artefacts
                .iter()
                .any(|a| matches!(a, buckle_core::task::Artefact::File { .. }))
        );
        assert!(
            FileTemplate
                .remove(&c, &record("file.template", "/etc/app.conf", &again))
                .await
                .unwrap()
                .changed
        );
    }

    fn cx_with_previous(
        sys: &FakeSystem,
        value: &str,
        previous: buckle_core::state::ResourceRecord,
    ) -> Cx {
        buckle_core::testing::CxBuilder::new(sys.clone())
            .source(source_with_name())
            .secrets(secrets("token", value))
            .previous(previous)
            .build()
    }

    #[tokio::test]
    async fn a_rotated_secret_shows_both_fingerprints_on_its_line_and_neither_value() {
        const OLD: &str = "old-secret-value-1";
        const NEW: &str = "new-secret-value-2";
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "secret.conf.j2", "dest": "/etc/app.conf"}));
        let first = apply(
            &FileTemplate,
            &cx_secrets(&sys, source_with_name(), secrets("token", OLD)),
            &s,
        )
        .await;
        let old_token = first
            .shadow
            .as_ref()
            .unwrap()
            .get()
            .trim()
            .trim_start_matches("token = ")
            .to_owned();
        let rotated = cx_with_previous(&sys, NEW, record("file.template", "/etc/app.conf", &first));
        let planned = FileTemplate.plan(&rotated, &s).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        let text = u.get();
        let minus: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .collect();
        let plus: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert_eq!(
            minus,
            vec![format!("-token = {old_token}").as_str()],
            "{text}"
        );
        assert_eq!(plus.len(), 1, "{text}");
        assert!(
            plus[0].contains("\u{ab}secret:token@") && plus[0] != minus[0].replacen('-', "+", 1)
        );
        let json = serde_json::to_string(&planned).unwrap();
        assert!(!json.contains(OLD) && !json.contains(NEW), "{json}");

        // A sensitive file new on disk is created 0600 when no mode was given.
        let meta = buckle_core::system::System::stat(&sys, std::path::Path::new("/etc/app.conf"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.mode, Some(0o600));
    }

    #[tokio::test]
    async fn once_a_file_held_secrets_its_disk_content_is_never_diffed() {
        // The template no longer uses the secret, but the file on disk still holds it.
        const VALUE: &str = "still-on-disk-secret";
        let sys = FakeSystem::new();
        let s: Spec = spec(json!({"src": "secret.conf.j2", "dest": "/etc/app.conf"}));
        let first = apply(
            &FileTemplate,
            &cx_secrets(&sys, source_with_name(), secrets("token", VALUE)),
            &s,
        )
        .await;
        let plain = buckle_core::testing::CxBuilder::new(sys.clone())
            .source(MemorySource::new().with("secret.conf.j2", "token = none\n"))
            .previous(record("file.template", "/etc/app.conf", &first))
            .build();
        let planned = FileTemplate.plan(&plain, &s).await.unwrap();
        let json = serde_json::to_string(&planned).unwrap();
        assert!(!json.contains(VALUE), "{json}");
        assert!(
            json.contains("secret:token@"),
            "the old side is the redacted shadow: {json}"
        );

        // Drifted since Buckle wrote it: only a digest summary.
        put(
            &sys,
            "/etc/app.conf",
            &format!("token = {VALUE}\nextra = 1\n"),
        )
        .await;
        let planned = FileTemplate.plan(&plain, &s).await.unwrap();
        let json = serde_json::to_string(&planned).unwrap();
        assert!(!json.contains(VALUE) && json.contains("digest"), "{json}");
    }
}
