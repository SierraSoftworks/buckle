//! `ssh.authorized_key`: manage entries in a user's `authorized_keys`.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Manages public keys in a user's `~/.ssh/authorized_keys`.
///
/// ```yaml
/// - ssh.authorized_key:
///     user: deploy
///     key: |
///       ssh-ed25519 AAAAC3Nza... laptop
///       from="10.0.0.0/8" ssh-ed25519 AAAAC3Nza... ci
///     state: present        # or absent
///     exclusive: false      # true: the file contains exactly these keys
/// ```
///
/// `key` is one or more lines in `authorized_keys` format (blank lines and
/// `#` comments are ignored). A key is identified by its type and base64
/// body, so changing its options or comment updates the existing line rather
/// than adding a second one. With `state: absent` the listed keys are deleted.
/// With `exclusive: true` (and `state: present`) every other key is deleted.
///
/// The `.ssh` directory is kept at mode 0700 and the file at 0600, owned by the
/// user (and by their private group when one with the user's name exists). If
/// the user does not exist yet (an earlier task creates it) the plan assumes
/// `/home/<user>` (`/root` for root). Removal deletes the key lines Buckle
/// added and leaves the rest of the file, including an emptied file, alone.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    pub user: String,
    /// One or more public key lines.
    pub key: String,
    #[serde(default)]
    pub state: Ensure,
    /// Remove every key not listed (only with `state: present`).
    #[serde(default)]
    pub exclusive: bool,
}

pub struct SshAuthorizedKey;
buckle_core::register_task!(SshAuthorizedKey);

const KEY_TYPES: &[&str] = &["ssh-", "ecdsa-", "sk-ssh-", "sk-ecdsa-"];

/// The `(type, base64 body)` identity of an authorized_keys line, ignoring options and comment.
fn identity(line: &str) -> Option<(String, String)> {
    let tokens = split_tokens(line);
    let at = tokens
        .iter()
        .position(|t| KEY_TYPES.iter().any(|p| t.starts_with(p)))?;
    Some((tokens[at].clone(), tokens.get(at + 1)?.clone()))
}

/// Splits on whitespace, but not inside double quotes (options can hold spaces).
fn split_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn wanted_keys(spec: &Spec) -> Result<Vec<(String, (String, String))>> {
    let keys: Vec<_> = spec
        .key
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            identity(l).map(|id| (l.to_owned(), id)).ok_or_else(|| {
                error::user(
                    "A line in `key` is not an SSH public key.",
                    &["Use lines such as `ssh-ed25519 AAAA... comment`."],
                )
            })
        })
        .collect::<Result<_>>()?;
    if keys.is_empty() {
        return Err(error::user(
            "The `key` of ssh.authorized_key contains no keys.",
            &["Provide at least one public key line."],
        ));
    }
    Ok(keys)
}

/// The result of applying a spec to the current lines.
struct Edited {
    lines: Vec<String>,
    /// Lines Buckle added or rewrote (for the plan summary).
    touched: Vec<String>,
    /// Identities of keys that were not in the file and that Buckle appended.
    appended: Vec<(String, String)>,
}

/// Applies the spec to the current lines.
fn edit(spec: &Spec, current: &[String]) -> Result<Edited> {
    let wanted = wanted_keys(spec)?;
    let mut lines = current.to_vec();
    let mut touched = Vec::new();
    let mut appended = Vec::new();
    match spec.state {
        Ensure::Absent => {
            lines.retain(|l| identity(l).is_none_or(|id| !wanted.iter().any(|(_, w)| *w == id)));
        }
        Ensure::Present => {
            if spec.exclusive {
                lines.retain(|l| identity(l).is_none_or(|id| wanted.iter().any(|(_, w)| *w == id)));
            }
            for (line, id) in &wanted {
                match lines.iter().position(|l| identity(l).as_ref() == Some(id)) {
                    Some(i) if lines[i] == *line => {}
                    Some(i) => {
                        lines[i] = line.clone();
                        touched.push(line.clone());
                    }
                    None => {
                        lines.push(line.clone());
                        touched.push(line.clone());
                        appended.push(id.clone());
                    }
                }
            }
        }
    }
    Ok(Edited {
        lines,
        touched,
        appended,
    })
}

/// The key identities this resource added on earlier applies, by file.
fn owned_ids(cx: &Cx, file: &std::path::Path) -> Vec<(String, String)> {
    hostkit::ownership::previous_artefacts(cx)
        .iter()
        .filter_map(|a| match a {
            Artefact::Line { path, marker } if path == file => identity(marker),
            _ => None,
        })
        .collect()
}

struct Target {
    ssh_dir: PathBuf,
    file: PathBuf,
    owner: String,
    group: Option<String>,
}

impl SshAuthorizedKey {
    async fn target(&self, cx: &Cx, spec: &Spec) -> Result<Target> {
        super::user_group::valid_name(&spec.user)?;
        let home = match cx.system.lookup_user(&spec.user).await? {
            Some(user) => user.home,
            None if spec.user == "root" => PathBuf::from("/root"),
            None => PathBuf::from(format!("/home/{}", spec.user)),
        };
        let group = match (
            cx.system.lookup_user(&spec.user).await?,
            cx.system.lookup_group(&spec.user).await?,
        ) {
            (Some(user), Some(group)) if group.gid == user.gid => Some(group.name),
            _ => None,
        };
        Ok(Target {
            ssh_dir: home.join(".ssh"),
            file: home.join(".ssh/authorized_keys"),
            owner: spec.user.clone(),
            group,
        })
    }
}

fn file_perms(t: &Target) -> FilePerms {
    FilePerms {
        mode: Some(0o600),
        owner: Some(t.owner.clone()),
        group: t.group.clone(),
    }
}

fn dir_perms(t: &Target) -> FilePerms {
    FilePerms {
        mode: Some(0o700),
        ..file_perms(t)
    }
}

struct Inspection {
    planned: Planned,
    target: Target,
    new_text: String,
    new_lines: Vec<String>,
    appended: Vec<(String, String)>,
    file_exists: bool,
    dir_exists: bool,
    content_changed: bool,
}

impl SshAuthorizedKey {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        let target = self.target(cx, spec).await?;
        let dir_meta = cx.system.stat(&target.ssh_dir).await?;
        let file = hostkit::load_text(cx, &target.file).await?;
        let current_lines = file
            .as_ref()
            .map(|f| hostkit::split_lines(&f.text))
            .unwrap_or_default();
        let Edited {
            lines,
            touched,
            appended,
        } = edit(spec, &current_lines)?;
        let new_text = hostkit::join_lines(&lines);
        let old_text = file.as_ref().map(|f| f.text.clone()).unwrap_or_default();
        let content_changed = lines != current_lines;
        let dir_exists = dir_meta
            .as_ref()
            .is_some_and(|m| m.kind == FileKind::Directory);
        if dir_meta
            .as_ref()
            .is_some_and(|m| m.kind != FileKind::Directory)
        {
            return Err(error::user(
                format!(
                    "{} exists but is not a directory.",
                    target.ssh_dir.display()
                ),
                &["Move the file aside so that Buckle can create the .ssh directory."],
            ));
        }
        let mut perm_notes = Vec::new();
        if spec.state == Ensure::Present {
            if let Some(m) = &dir_meta {
                perm_notes.extend(
                    hostkit::perm_changes(m, &dir_perms(&target))
                        .into_iter()
                        .map(|c| format!(".ssh {c}")),
                );
            }
            if let Some(f) = &file {
                perm_notes.extend(
                    hostkit::perm_changes(&f.meta, &file_perms(&target))
                        .into_iter()
                        .map(|c| format!("authorized_keys {c}")),
                );
            }
        }
        let path = target.file.display();
        let planned = if (spec.state == Ensure::Absent && file.is_none())
            || (!content_changed && perm_notes.is_empty())
        {
            Planned::NoOp
        } else if file.is_none() {
            Planned::change(Diff::Create {
                summary: format!("create {path} with {} key(s)", touched.len()),
                preview: Some(buckle_core::secrets::Redacted::plain(new_text.clone())),
            })
        } else if content_changed {
            hostkit::text_diff_planned(format!("update {path}"), &old_text, &new_text)
        } else {
            Planned::change(Diff::Update {
                summary: format!("{path}: {}", perm_notes.join(", ")),
                unified: None,
            })
        };
        Ok(Inspection {
            planned,
            target,
            new_text,
            new_lines: lines,
            appended,
            file_exists: file.is_some(),
            dir_exists,
            content_changed,
        })
    }
}

#[async_trait]
impl Task for SshAuthorizedKey {
    type Spec = Spec;
    const KIND: &'static str = "ssh.authorized_key";
    const PLATFORMS: Platforms = Platforms::UNIX;
    const DESCRIPTION: &'static str =
        "Manage public keys in a user's ~/.ssh/authorized_keys with correct permissions.";

    fn key(spec: &Spec) -> String {
        match wanted_keys(spec) {
            Ok(keys) if keys.len() == 1 => {
                let line = &keys[0].0;
                let comment = split_tokens(line)
                    .into_iter()
                    .skip_while(|t| !KEY_TYPES.iter().any(|p| t.starts_with(p)))
                    .nth(2)
                    .unwrap_or_else(|| keys[0].1.1.chars().take(16).collect());
                format!("{}: {comment}", spec.user)
            }
            Ok(keys) => format!(
                "{}: {} keys ({})",
                spec.user,
                keys.len(),
                Digest::of(spec.key.trim().as_bytes()).short()
            ),
            Err(_) => spec.user.clone(),
        }
    }

    fn describe(spec: &Spec) -> String {
        format!("authorized keys for {}", spec.user)
    }

    #[instrument(name = "task.ssh.authorized_key.plan", skip_all, fields(user = %spec.user, state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.ssh.authorized_key.apply", skip_all, fields(user = %spec.user, state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let i = self.inspect(cx, spec).await?;
        if !i.planned.is_change() {
            return Ok(hostkit::ownership::carry_forward(
                cx,
                Applied::unchanged(format!("keys for {} already up to date", spec.user)),
            ));
        }
        let path = &i.target.file;
        if spec.state == Ensure::Present {
            if !i.dir_exists {
                cx.system
                    .mkdir_all(&i.target.ssh_dir, &dir_perms(&i.target))
                    .await?;
            } else {
                cx.system
                    .set_perms(&i.target.ssh_dir, &dir_perms(&i.target))
                    .await?;
            }
        }
        if i.content_changed {
            let perms = if spec.state == Ensure::Present || !i.file_exists {
                file_perms(&i.target)
            } else {
                hostkit::perms_of(&cx.system.stat(path).await?.ok_or_else(|| {
                    error::user(
                        format!("{} disappeared during the edit.", path.display()),
                        &["Run the apply again."],
                    )
                })?)
            };
            cx.system
                .write_atomic(path, i.new_text.as_bytes(), &perms)
                .await?;
        } else {
            cx.system.set_perms(path, &file_perms(&i.target)).await?;
        }
        // Buckle owns only the keys it appended, on this apply or an earlier one.
        // A key that was already in the file stays the user's even when Buckle
        // rewrote its options, so removal never deletes it.
        let mut owned = owned_ids(cx, path);
        owned.extend(i.appended.iter().cloned());
        let mut applied = Applied::changed(format!("updated {}", path.display()));
        for line in &i.new_lines {
            if let Some(id) = identity(line)
                && owned.contains(&id)
                && !applied
                    .artefacts
                    .iter()
                    .any(|a| matches!(a, Artefact::Line { marker, .. } if identity(marker).as_ref() == Some(&id)))
            {
                applied = applied.with_artefact(Artefact::Line {
                    path: path.clone(),
                    marker: line.clone(),
                });
            }
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let n = record
            .artefacts
            .iter()
            .filter(|a| matches!(a, Artefact::Line { .. }))
            .count();
        format!(
            "delete the {n} key line(s) Buckle added for {}",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let mut by_file: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for artefact in &record.artefacts {
            if let Artefact::Line { path, marker } = artefact {
                match by_file.iter_mut().find(|(p, _)| p == path) {
                    Some((_, lines)) => lines.push(marker.clone()),
                    None => by_file.push((path.clone(), vec![marker.clone()])),
                }
            }
        }
        if by_file.is_empty() {
            return Ok(Applied::skipped(
                "no keys were added by Buckle, nothing to remove",
            ));
        }
        let mut removed = 0;
        for (path, markers) in by_file {
            let Some(file) = hostkit::load_text(cx, &path).await? else {
                continue;
            };
            let ids: Vec<_> = markers.iter().filter_map(|m| identity(m)).collect();
            let mut lines = hostkit::split_lines(&file.text);
            let before = lines.len();
            lines.retain(|l| identity(l).is_none_or(|id| !ids.contains(&id)));
            removed += before - lines.len();
            if lines.len() != before {
                hostkit::save_text(
                    cx,
                    &path,
                    &hostkit::join_lines(&lines),
                    Some(&file),
                    false,
                    &FilePerms::default(),
                )
                .await?;
            }
        }
        Ok(if removed == 0 {
            Applied::unchanged("the keys are no longer present")
        } else {
            Applied::changed(format!("removed {removed} key line(s)"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::{GroupInfo, System, UserInfo};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;
    use std::path::Path;

    const K1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA laptop";
    const K2: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABBBB ci";
    const FILE: &str = "/home/deploy/.ssh/authorized_keys";

    fn host() -> FakeSystem {
        FakeSystem::new()
            .with_user(UserInfo {
                name: "deploy".into(),
                uid: 1001,
                gid: 1001,
                home: "/home/deploy".into(),
                shell: "/bin/bash".into(),
                gecos: String::new(),
            })
            .with_group(GroupInfo {
                name: "deploy".into(),
                gid: 1001,
                members: vec![],
            })
    }

    fn with_keys(keys: &str) -> FakeSystem {
        host()
            .with_dir(
                "/home/deploy/.ssh",
                FilePerms {
                    mode: Some(0o700),
                    owner: Some("deploy".into()),
                    group: Some("deploy".into()),
                },
            )
            .with_file(
                FILE,
                keys,
                FilePerms {
                    mode: Some(0o600),
                    owner: Some("deploy".into()),
                    group: Some("deploy".into()),
                },
            )
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"user": "deploy", "key": K1});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[rstest]
    #[case::no_file(host(), json!({}), Class::Change)]
    #[case::has_key(with_keys(&format!("{K1}\n")), json!({}), Class::NoOp)]
    #[case::other_key_present(with_keys(&format!("{K2}\n")), json!({}), Class::Change)]
    #[case::comment_changed(with_keys("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA old-comment\n"), json!({}), Class::Change)]
    #[case::absent_when_present(with_keys(&format!("{K1}\n")), json!({"state": "absent"}), Class::Change)]
    #[case::absent_when_not(with_keys(&format!("{K2}\n")), json!({"state": "absent"}), Class::NoOp)]
    #[case::absent_without_file(host(), json!({"state": "absent"}), Class::NoOp)]
    #[case::exclusive_with_extras(with_keys(&format!("{K1}\n{K2}\n")), json!({"exclusive": true}), Class::Change)]
    #[case::exclusive_exact(with_keys(&format!("{K1}\n")), json!({"exclusive": true}), Class::NoOp)]
    #[case::bad_file_mode(host().with_file(FILE, format!("{K1}\n"), FilePerms::mode(0o644)), json!({}), Class::Change)]
    #[case::unknown_user_assumes_home(FakeSystem::new(), json!({}), Class::Change)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        assert_eq!(
            plan_class(&SshAuthorizedKey, &cx(&sys), &s(extra)).await,
            expected
        );
    }

    #[tokio::test]
    async fn adding_a_key_creates_the_directory_and_file_with_strict_modes() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(json!({}));
        let applied = apply(&SshAuthorizedKey, &c, &spec).await;
        assert_eq!(sys.file_string(FILE).unwrap(), format!("{K1}\n"));
        let dir = sys
            .stat(Path::new("/home/deploy/.ssh"))
            .await
            .unwrap()
            .unwrap();
        let file = sys.stat(Path::new(FILE)).await.unwrap().unwrap();
        assert_eq!(
            (dir.mode, dir.owner.as_deref(), dir.group.as_deref()),
            (Some(0o700), Some("deploy"), Some("deploy"))
        );
        assert_eq!(
            (file.mode, file.owner.as_deref()),
            (Some(0o600), Some("deploy"))
        );
        assert_eq!(applied.artefacts.len(), 1);
        assert_eq!(plan_class(&SshAuthorizedKey, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn existing_keys_are_kept_and_updates_replace_in_place() {
        let sys = with_keys(&format!(
            "# mine\n{K2}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA old\n"
        ));
        let c = cx(&sys);
        apply(
            &SshAuthorizedKey,
            &c,
            &s(json!({"key": format!("from=\"10.0.0.1\" {K1}")})),
        )
        .await;
        assert_eq!(
            sys.file_string(FILE).unwrap(),
            format!("# mine\n{K2}\nfrom=\"10.0.0.1\" {K1}\n")
        );
    }

    #[tokio::test]
    async fn exclusive_and_absent_delete_keys() {
        let sys = with_keys(&format!("{K1}\n{K2}\n"));
        let c = cx(&sys);
        apply(&SshAuthorizedKey, &c, &s(json!({"exclusive": true}))).await;
        assert_eq!(sys.file_string(FILE).unwrap(), format!("{K1}\n"));
        apply(&SshAuthorizedKey, &c, &s(json!({"state": "absent"}))).await;
        assert_eq!(sys.file_string(FILE).unwrap(), "");
    }

    #[tokio::test]
    async fn multiple_keys_in_one_task() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(json!({"key": format!("{K1}\n\n# comment\n{K2}\n")}));
        apply(&SshAuthorizedKey, &c, &spec).await;
        assert_eq!(sys.file_string(FILE).unwrap(), format!("{K1}\n{K2}\n"));
        assert_eq!(plan_class(&SshAuthorizedKey, &c, &spec).await, Class::NoOp);
        assert!(SshAuthorizedKey::key(&spec).contains("2 keys"));
    }

    #[tokio::test]
    async fn removal_deletes_only_the_keys_buckle_added() {
        let sys = with_keys(&format!("{K2}\n"));
        let c = cx(&sys);
        let rec = record(
            "ssh.authorized_key",
            "k",
            &apply(&SshAuthorizedKey, &c, &s(json!({}))).await,
        );
        assert_eq!(sys.file_string(FILE).unwrap(), format!("{K2}\n{K1}\n"));
        assert!(SshAuthorizedKey.remove(&c, &rec).await.unwrap().changed);
        assert_eq!(sys.file_string(FILE).unwrap(), format!("{K2}\n"));
        assert!(!SshAuthorizedKey.remove(&c, &rec).await.unwrap().changed);
    }

    #[tokio::test]
    async fn reoptioning_a_preexisting_key_never_claims_it_and_added_keys_stay_owned() {
        // K1 was in the file before Buckle; K2 is added by Buckle.
        let sys = with_keys(&format!("{K1}\n"));
        let both = s(json!({"key": format!("from=\"10.0.0.1\" {K1}\n{K2}")}));
        let first = apply(&SshAuthorizedKey, &cx(&sys), &both).await;
        assert_eq!(
            first.artefacts,
            vec![Artefact::Line {
                path: FILE.into(),
                marker: K2.into()
            }],
            "only the appended key is Buckle's"
        );
        let rec = record("ssh.authorized_key", "k", &first);

        // A later apply that only re-options K2 keeps it owned (with the new text).
        let reoptioned = s(json!({"key": format!("from=\"10.0.0.1\" {K1}\nno-pty {K2}")}));
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .previous(rec)
            .build();
        let second = apply(&SshAuthorizedKey, &c, &reoptioned).await;
        assert_eq!(
            second.artefacts,
            vec![Artefact::Line {
                path: FILE.into(),
                marker: format!("no-pty {K2}")
            }]
        );

        // Removal deletes K2 only; the pre-existing K1 stays (with its options).
        let rec = record("ssh.authorized_key", "k", &second);
        assert!(
            SshAuthorizedKey
                .remove(&cx(&sys), &rec)
                .await
                .unwrap()
                .changed
        );
        assert_eq!(
            sys.file_string(FILE).unwrap(),
            format!("from=\"10.0.0.1\" {K1}\n")
        );
    }

    #[rstest]
    #[case::not_a_key("hello world")]
    #[case::empty("  \n# only a comment\n")]
    #[tokio::test]
    async fn invalid_keys_are_user_errors(#[case] key: &str) {
        let sys = host();
        let err = SshAuthorizedKey
            .plan(&cx(&sys), &s(json!({"key": key})))
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[test]
    fn identity_ignores_options_and_comments() {
        assert_eq!(
            identity(&format!("command=\"a b\",no-pty {K1}")),
            identity(K1)
        );
        assert_eq!(identity("garbage"), None);
    }
}
