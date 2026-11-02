//! `file.template`: render a Jinja template from the module and write it to the host.

use super::hostkit::{self, describe_perms, diffable_text, digest_change, effective_perms};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::FileSink;
use buckle_core::secrets::{Redacted, Rendered};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Cx, Mode, Task};
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
/// replaces the destination; `{path}` is replaced with a temporary file next
/// to `dest`. A non-zero exit fails the task and leaves `dest` untouched.
/// When the file contains secrets, the validator's output is not echoed.
///
/// Removal deletes `dest` only if its content still matches what Buckle wrote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// Template path, relative to the module's `files/` directory.
    pub src: String,
    /// Absolute destination path on the host.
    pub dest: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// A command that checks the rendered file; `{path}` stands for the file to check.
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
            // The old content is only shown when the new render is secret-free: a
            // sensitive file that drifted would otherwise be read back into a plan.
            match (&rendered, diffable_text(&current)) {
                (Rendered::Plain(_), Some(old)) => Planned::change(Diff::unified(
                    summary,
                    &Redacted::plain(old.to_owned()),
                    &redacted,
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

    /// Runs the `validate` command against the real render in a temporary file next to `dest`.
    async fn validate(&self, cx: &Cx, spec: &Spec, rendered: &Rendered) -> Result<()> {
        let Some(argv) = &spec.validate else {
            return Ok(());
        };
        if !argv.iter().any(|a| a.contains("{path}")) {
            return Err(error::user(
                "The `validate` command does not contain the `{path}` placeholder.",
                &[
                    "Add `{path}` where the command takes the file to check, e.g. [\"nginx\", \"-t\", \"-c\", \"{path}\"].",
                ],
            ));
        }
        let parent = spec.dest.parent().unwrap_or(std::path::Path::new("/"));
        if cx.mode == Mode::Plan && cx.system.stat(parent).await?.is_none() {
            // Validating would mean creating the directory; apply will validate.
            return Ok(());
        }
        let tmp = hostkit::sibling_temp(&spec.dest, "buckle-validate");
        let perms = FilePerms {
            mode: Some(spec.perms.mode.unwrap_or(0o600)),
            owner: None,
            group: None,
        };
        FileSink
            .write_rendered(&*cx.system, &tmp, rendered, &perms)
            .await?;
        let substituted: Vec<String> = argv
            .iter()
            .map(|a| a.replace("{path}", &tmp.to_string_lossy()))
            .collect();
        let outcome = match Command::from_argv(&substituted) {
            Some(command) => cx.system.run(command).await,
            None => Err(error::user(
                "The `validate` command is empty.",
                &["Give the program and its arguments as a list."],
            )),
        };
        // Always clean up, whatever the validator said.
        let _ = cx.system.remove(&tmp, false).await;
        let out = outcome?;
        if out.success() {
            return Ok(());
        }
        let detail = if rendered.is_sensitive() {
            "Its output is withheld because the file contains secrets.".to_owned()
        } else {
            format!("stderr: {}", hostkit::output_tail(&out.stderr))
        };
        Err(error::user(
            format!(
                "The rendered content for {} failed validation: `{}` {}. {detail}",
                spec.dest.display(),
                substituted.join(" "),
                match out.code {
                    Some(c) => format!("exited with status {c}"),
                    None => "did not exit cleanly".to_owned(),
                },
            ),
            &[
                "Fix the template or the variables it uses; the destination file was not changed.",
                "Run the validator by hand against the rendered file to see the full message.",
            ],
        ))
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
            self.validate(cx, spec, &inspection.rendered).await?;
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
            self.validate(cx, spec, &inspection.rendered).await?;
            let existing_meta = inspection.existing.as_ref().map(|(m, _)| m);
            if spec.backup
                && let Some((meta, current)) = &inspection.existing
            {
                hostkit::backup_file(cx, dest, meta, current).await?;
            }
            let perms = effective_perms(&spec.perms, existing_meta);
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
                path.replace('\\', "/").starts_with("/etc/app/"),
                "the temp file is next to dest: {path}"
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
                .all(|p| !p.to_string_lossy().contains("buckle-validate"))
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
}
