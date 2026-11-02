//! `systemd.dropin`: a drop-in override file for a unit.

use super::sys_exec::{file_matches_digest, run_checked};
use super::systemd_common::systemctl;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Writes `/etc/systemd/system/<unit>.d/<name>` and reloads systemd.
///
/// `content` is a template string (rendered with the host context, secrets
/// are not allowed) and becomes the whole drop-in file, e.g.
/// `[Service]\nLimitNOFILE=65536\n`. `name` defaults to `override.conf` and
/// must end in `.conf`. `perms` sets mode, owner and group (default `0644`).
///
/// Applying writes the file and runs `systemctl daemon-reload`. The unit is
/// not restarted: notify a `systemd.restart` handler for that. Removal deletes
/// the drop-in (and its `.d` directory when it is left empty), unless the file
/// was edited by hand since Buckle wrote it, then reloads systemd.
pub struct SystemdDropin;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The unit to override, e.g. `nomad.service`.
    pub unit: String,
    /// Drop-in file name; defaults to `override.conf`.
    #[serde(default = "default_name")]
    pub name: String,
    /// The drop-in's content (a template string).
    pub content: String,
    /// Mode, owner and group of the file.
    #[serde(default)]
    pub perms: FilePerms,
}

fn default_name() -> String {
    "override.conf".into()
}

impl Spec {
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/system/{}.d/{}", self.unit, self.name))
    }

    fn validate(&self) -> Result<()> {
        let bad = |what: &str| {
            buckle_core::error::user(
                format!("systemd.dropin: {what}"),
                &["See `buckle catalog systemd.dropin` for the fields this task accepts."],
            )
        };
        if self.unit.is_empty() || self.unit.contains('/') {
            return Err(bad("`unit` must be a unit name such as nomad.service."));
        }
        if self.name.contains('/') || !self.name.ends_with(".conf") {
            return Err(bad("`name` must be a file name ending in .conf."));
        }
        Ok(())
    }

    fn render(&self, cx: &Cx) -> Result<String> {
        let mut content = cx.render_plain(&self.content)?;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        Ok(content)
    }

    fn perms(&self) -> FilePerms {
        FilePerms {
            mode: self.perms.mode.or(Some(0o644)),
            ..self.perms.clone()
        }
    }
}

#[async_trait]
impl Task for SystemdDropin {
    type Spec = Spec;
    const KIND: &'static str = "systemd.dropin";
    const LOCKS: &'static [&'static str] = &["systemd"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Write a systemd drop-in override (<unit>.d/<name>.conf) and reload systemd.";

    fn key(spec: &Spec) -> String {
        spec.path().display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        format!("{} drop-in {}", spec.unit, spec.name)
    }

    #[tracing::instrument(name = "task.systemd.dropin.plan", skip_all, fields(unit = %spec.unit))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        spec.validate()?;
        let wanted = spec.render(cx)?;
        let path = spec.path();
        let summary = format!("drop-in {}", path.display());
        let Some(current) = cx.system.read(&path).await? else {
            return Ok(Planned::change(Diff::Create {
                summary,
                preview: Some(Redacted::plain(wanted)),
            }));
        };
        let current = String::from_utf8_lossy(&current).into_owned();
        if current != wanted {
            return Ok(Planned::change(Diff::unified(
                summary,
                &Redacted::plain(current),
                &Redacted::plain(wanted),
            )));
        }
        if let Some(meta) = cx.system.stat(&path).await? {
            let perms = spec.perms();
            let mut drift = Vec::new();
            if perms.mode.is_some() && meta.mode.map(|m| m & 0o7777) != perms.mode {
                drift.push("mode");
            }
            if perms.owner.is_some() && meta.owner != perms.owner {
                drift.push("owner");
            }
            if perms.group.is_some() && meta.group != perms.group {
                drift.push("group");
            }
            if !drift.is_empty() {
                return Ok(Planned::change(Diff::Opaque {
                    summary: format!("fix {} of {}", drift.join(" and "), path.display()),
                }));
            }
        }
        Ok(Planned::NoOp)
    }

    #[tracing::instrument(name = "task.systemd.dropin.apply", skip_all, fields(unit = %spec.unit))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        spec.validate()?;
        let content = spec.render(cx)?;
        let path = spec.path();
        cx.system
            .write_atomic(&path, content.as_bytes(), &spec.perms())
            .await?;
        // write_atomic only applies perms on creation in some implementations.
        cx.system.set_perms(&path, &spec.perms()).await?;
        run_checked(cx, systemctl(&["daemon-reload"])).await?;
        Ok(Applied::changed(format!("wrote {}", path.display()))
            .with_shadow(
                Redacted::plain(content.clone()),
                Digest::of(content.as_bytes()),
            )
            .with_artefact(Artefact::File { path }))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!("delete drop-in {} and reload systemd", record.key.key)
    }

    #[tracing::instrument(name = "task.systemd.dropin.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let path = record
            .artefacts
            .iter()
            .find_map(|a| match a {
                Artefact::File { path } => Some(path.clone()),
                _ => None,
            })
            .unwrap_or_else(|| PathBuf::from(&record.key.key));
        if file_matches_digest(cx, &path, record.content_digest.as_ref()).await? == Some(false) {
            return Ok(Applied::skipped(format!(
                "{} was modified since Buckle wrote it; left in place",
                path.display()
            )));
        }
        if !cx.system.remove(&path, false).await? {
            return Ok(Applied::unchanged(format!(
                "{} was already removed",
                path.display()
            )));
        }
        if let Some(dir) = path.parent()
            && let Some(entries) = cx.system.list_dir(dir).await?
            && entries.is_empty()
        {
            cx.system.remove(dir, false).await?;
        }
        run_checked(cx, systemctl(&["daemon-reload"])).await?;
        Ok(Applied::changed(format!("removed {}", path.display())))
    }
}

buckle_core::register_task!(SystemdDropin);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host() -> FakeSystem {
        FakeSystem::new().on_command_ok("systemctl", &["daemon-reload"], "")
    }

    fn nomad() -> serde_json::Value {
        json!({"unit": "nomad.service", "content": "[Service]\nLimitNOFILE=65536"})
    }

    const PATH: &str = "/etc/systemd/system/nomad.service.d/override.conf";

    #[tokio::test]
    async fn plan_creates_then_converges_after_apply() {
        let sys = host();
        let cx = cx(&sys);
        let s: Spec = spec(nomad());
        match SystemdDropin.plan(&cx, &s).await.unwrap() {
            Planned::Change {
                diff: Diff::Create { preview, .. },
            } => {
                assert_eq!(preview.unwrap().get(), "[Service]\nLimitNOFILE=65536\n");
            }
            other => panic!("expected create, got {other:?}"),
        }
        let applied = SystemdDropin.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(
            sys.file_string(PATH).as_deref(),
            Some("[Service]\nLimitNOFILE=65536\n")
        );
        assert_eq!(sys.command_lines(), vec!["systemctl daemon-reload"]);
        assert_eq!(
            applied.artefacts,
            vec![Artefact::File { path: PATH.into() }]
        );
        assert_eq!(SystemdDropin.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn plan_diffs_changed_content() {
        let sys = host().with_file(
            PATH,
            "[Service]\nLimitNOFILE=1024\n",
            FilePerms::mode(0o644),
        );
        let cx = cx(&sys);
        match SystemdDropin.plan(&cx, &spec(nomad())).await.unwrap() {
            Planned::Change {
                diff: Diff::Update {
                    unified: Some(u), ..
                },
            } => {
                assert!(
                    u.get().contains("-LimitNOFILE=1024") && u.get().contains("+LimitNOFILE=65536")
                );
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_notices_wrong_permissions() {
        let sys = host().with_file(
            PATH,
            "[Service]\nLimitNOFILE=65536\n",
            FilePerms::mode(0o600),
        );
        let cx = cx(&sys);
        let planned = SystemdDropin.plan(&cx, &spec(nomad())).await.unwrap();
        assert!(matches!(
            planned,
            Planned::Change {
                diff: Diff::Opaque { .. }
            }
        ));
        let s: Spec = spec(
            json!({"unit": "nomad.service", "content": "[Service]\nLimitNOFILE=65536\n", "perms": {"mode": "0600"}}),
        );
        assert_eq!(SystemdDropin.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn content_is_rendered_with_the_host_context() {
        let sys = host();
        let cx = cx(&sys);
        let s: Spec = spec(
            json!({"unit": "nomad.service", "name": "limits.conf", "content": "[Service]\nEnvironment=HOST={{ host.id }}\n"}),
        );
        SystemdDropin.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(
            sys.file_string("/etc/systemd/system/nomad.service.d/limits.conf")
                .as_deref(),
            Some("[Service]\nEnvironment=HOST=test-host\n")
        );
    }

    #[rstest]
    #[case(json!({"unit": "", "content": "x"}))]
    #[case(json!({"unit": "a/b.service", "content": "x"}))]
    #[case(json!({"unit": "a.service", "name": "x.txt", "content": "x"}))]
    #[case(json!({"unit": "a.service", "name": "../x.conf", "content": "x"}))]
    #[tokio::test]
    async fn invalid_specs_are_user_errors(#[case] j: serde_json::Value) {
        let cx = cx(&host());
        let err = SystemdDropin.plan(&cx, &spec(j)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[tokio::test]
    async fn removal_deletes_the_file_the_empty_directory_and_reloads() {
        let sys = host();
        let cx = cx(&sys);
        let applied = SystemdDropin
            .apply(&cx, &spec(nomad()), &Planned::NoOp)
            .await
            .unwrap();
        let record = record_of("systemd.dropin", PATH, &applied);
        let removed = SystemdDropin.remove(&cx, &record).await.unwrap();
        assert!(removed.changed);
        assert!(!sys.exists(PATH));
        assert!(!sys.exists("/etc/systemd/system/nomad.service.d"));
        assert_eq!(
            sys.command_lines(),
            vec!["systemctl daemon-reload", "systemctl daemon-reload"]
        );
    }

    #[tokio::test]
    async fn removal_keeps_a_directory_with_other_drop_ins_and_edited_files() {
        let sys = host().with_file(
            "/etc/systemd/system/nomad.service.d/other.conf",
            "[Service]\n",
            FilePerms::default(),
        );
        let cx = cx(&sys);
        let applied = SystemdDropin
            .apply(&cx, &spec(nomad()), &Planned::NoOp)
            .await
            .unwrap();
        let record = record_of("systemd.dropin", PATH, &applied);
        SystemdDropin.remove(&cx, &record).await.unwrap();
        assert!(sys.exists("/etc/systemd/system/nomad.service.d/other.conf"));

        // Hand edits are left alone.
        let applied = SystemdDropin
            .apply(&cx, &spec(nomad()), &Planned::NoOp)
            .await
            .unwrap();
        let record = record_of("systemd.dropin", PATH, &applied);
        sys.write_atomic(
            std::path::Path::new(PATH),
            b"edited\n",
            &FilePerms::default(),
        )
        .await
        .unwrap();
        let removed = SystemdDropin.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed && removed.summary.contains("modified"));
        assert!(sys.exists(PATH));
    }

    use buckle_core::system::System as _;
}
