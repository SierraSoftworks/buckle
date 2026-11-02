//! `user.account`: manage a local user account.

use super::hostkit::{self, Ensure};
use super::user_group::valid_name;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::ProcessEnvSink;
use buckle_core::secrets::{Rendered, SecretField};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, UserInfo};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Ensures a local user account exists with the given attributes (or is absent).
///
/// ```yaml
/// - user.account:
///     name: sierra
///     uid: 1001                      # optional
///     group: sierra                  # primary group (must exist)
///     groups: [sudo, docker]         # supplementary groups, appended (never removed)
///     shell: /bin/bash
///     home: /home/sierra
///     create_home: true              # default: true, or false for `system: true`
///     system: false                  # a system account (useradd -r)
///     comment: "Sierra Softworks"
///     password: "{{ secrets.sierra_password_hash }}"   # a crypt(3) hash, not plaintext
///     state: present                 # or absent
///     remove_home: false             # with state: absent, or when removed from the config
/// ```
///
/// Uses `useradd`, `usermod` and `userdel`. Only the attributes you set are
/// compared and changed; `groups` are added with `usermod -a -G` and never
/// removed. A `password` is a pre-hashed value (`openssl passwd -6`) and may
/// reference secrets: it is applied with `chpasswd -e`, which receives
/// `name:hash` on **stdin** (through the secret sink), so it never appears on
/// a command line, in the plan or in logs. The plan compares the hash with
/// `/etc/shadow` by digest; if the shadow file cannot be read the password is
/// assumed to be correct.
///
/// Removing the task from the configuration deletes the account (`userdel`,
/// with `-r` only when `remove_home: true`) if Buckle created it, and is
/// flagged as a cautionary removal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    pub name: String,
    #[serde(default)]
    pub uid: Option<u32>,
    /// Primary group.
    #[serde(default)]
    pub group: Option<String>,
    /// Supplementary groups to be a member of.
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// Create the home directory; defaults to true except for system accounts.
    #[serde(default)]
    pub create_home: Option<bool>,
    #[serde(default)]
    pub system: bool,
    /// A password hash (crypt format); may be a template referencing secrets.
    #[serde(default)]
    pub password: Option<SecretField>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub state: Ensure,
    /// Delete the home directory together with the account.
    #[serde(default)]
    pub remove_home: bool,
}

pub struct UserAccount;
buckle_core::register_task!(UserAccount);

const REMOVE_HOME: &str = "user.account: remove home directory";

/// The password hash recorded for `name` in /etc/shadow's digest form, when readable.
async fn shadow_digest(cx: &Cx, name: &str) -> Result<Option<Digest>> {
    let Some(bytes) = cx.system.read(std::path::Path::new("/etc/shadow")).await? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.lines().find_map(|line| {
        let mut parts = line.split(':');
        (parts.next() == Some(name)).then(|| Digest::of(parts.next().unwrap_or("").as_bytes()))
    }))
}

struct Changes {
    /// Human-readable list for the plan.
    summary: Vec<String>,
    usermod: Vec<String>,
    add_groups: Vec<String>,
    password: bool,
}

impl UserAccount {
    fn create_home(spec: &Spec) -> bool {
        spec.create_home.unwrap_or(!spec.system)
    }

    async fn changes(&self, cx: &Cx, spec: &Spec, user: &UserInfo) -> Result<Changes> {
        let mut c = Changes {
            summary: Vec::new(),
            usermod: Vec::new(),
            add_groups: Vec::new(),
            password: false,
        };
        if let Some(uid) = spec.uid
            && uid != user.uid
        {
            c.summary.push(format!("uid {} → {uid}", user.uid));
            c.usermod.extend(["-u".into(), uid.to_string()]);
        }
        if let Some(shell) = &spec.shell
            && std::path::Path::new(shell) != user.shell
        {
            c.summary
                .push(format!("shell {} → {shell}", user.shell.display()));
            c.usermod.extend(["-s".into(), shell.clone()]);
        }
        if let Some(home) = &spec.home
            && *home != user.home
        {
            c.summary
                .push(format!("home {} → {}", user.home.display(), home.display()));
            c.usermod.extend(["-d".into(), home.display().to_string()]);
        }
        if let Some(comment) = &spec.comment
            && *comment != user.gecos
        {
            c.summary.push("comment changed".into());
            c.usermod.extend(["-c".into(), comment.clone()]);
        }
        let mut primary_gid = user.gid;
        if let Some(group) = &spec.group {
            match cx.system.lookup_group(group).await? {
                Some(g) if g.gid == user.gid => {}
                Some(g) => {
                    c.summary.push(format!("primary group → {group}"));
                    c.usermod.extend(["-g".into(), group.clone()]);
                    primary_gid = g.gid;
                }
                None => {
                    // Perhaps an earlier task creates it; apply will fail clearly if not.
                    c.summary.push(format!("primary group → {group}"));
                    c.usermod.extend(["-g".into(), group.clone()]);
                }
            }
        }
        for group in &spec.groups {
            let member = match cx.system.lookup_group(group).await? {
                Some(g) => g.gid == primary_gid || g.members.contains(&spec.name),
                None => false,
            };
            if !member {
                c.add_groups.push(group.clone());
            }
        }
        if !c.add_groups.is_empty() {
            c.summary
                .push(format!("add to groups {}", c.add_groups.join(", ")));
        }
        if let Some(password) = &spec.password
            && let Some(current) = shadow_digest(cx, &spec.name).await?
            && current != password.resolve(&cx.templater, &cx.host)?.digest()
        {
            c.summary.push("password changed".into());
            c.password = true;
        }
        Ok(c)
    }

    fn useradd(&self, spec: &Spec) -> Command {
        let mut cmd = Command::new("useradd");
        if spec.system {
            cmd = cmd.arg("-r");
        }
        if let Some(uid) = spec.uid {
            cmd = cmd.arg("-u").arg(uid.to_string());
        }
        if let Some(group) = &spec.group {
            cmd = cmd.arg("-g").arg(group);
        }
        if !spec.groups.is_empty() {
            cmd = cmd.arg("-G").arg(spec.groups.join(","));
        }
        if let Some(shell) = &spec.shell {
            cmd = cmd.arg("-s").arg(shell);
        }
        if let Some(home) = &spec.home {
            cmd = cmd.arg("-d").arg(home.display().to_string());
        }
        if let Some(comment) = &spec.comment {
            cmd = cmd.arg("-c").arg(comment);
        }
        cmd = cmd.arg(if Self::create_home(spec) { "-m" } else { "-M" });
        cmd.arg(&spec.name)
    }

    /// Sets the password with `chpasswd -e`, feeding `name:hash` on stdin through the sink.
    async fn set_password(&self, cx: &Cx, spec: &Spec, password: &SecretField) -> Result<()> {
        hostkit::require_tool(cx, "chpasswd", "install the `passwd` package").await?;
        let line = SecretField::new(format!("{}:{}", spec.name, password.raw()));
        let mut cmd = Command::new("chpasswd").arg("-e");
        match line.resolve(&cx.templater, &cx.host)? {
            Rendered::Sensitive(secret) => ProcessEnvSink.stdin_sensitive(&mut cmd, &secret),
            Rendered::Plain(text) => {
                // A literal hash is still a credential: keep it out of captured output.
                cmd.stdin = Some(text.into_bytes());
                cmd.sensitive_env = true;
            }
        }
        hostkit::run_checked(cx, cmd, "Setting the password").await?;
        Ok(())
    }
}

#[async_trait]
impl Task for UserAccount {
    type Spec = Spec;
    const KIND: &'static str = "user.account";
    const LOCKS: &'static [&'static str] = &["users"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_CAUTION: bool = true;
    const DESCRIPTION: &'static str = "Ensure a local user account exists with the given uid, groups, shell, home and password hash.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        match spec.state {
            Ensure::Present => format!("user {}", spec.name),
            Ensure::Absent => format!("user {} absent", spec.name),
        }
    }

    #[instrument(name = "task.user.account.plan", skip_all, fields(user = %spec.name, state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        valid_name(&spec.name)?;
        let current = cx.system.lookup_user(&spec.name).await?;
        match (spec.state, current) {
            (Ensure::Absent, None) => Ok(Planned::NoOp),
            (Ensure::Absent, Some(_)) => {
                if let Some(b) =
                    hostkit::missing_tool(cx, "userdel", "install the `passwd` package").await?
                {
                    return Ok(b);
                }
                Ok(Planned::change(Diff::Delete {
                    summary: format!(
                        "delete user {}{}",
                        spec.name,
                        if spec.remove_home {
                            " and its home directory"
                        } else {
                            ""
                        }
                    ),
                }))
            }
            (Ensure::Present, None) => {
                if let Some(b) =
                    hostkit::missing_tool(cx, "useradd", "install the `passwd` package").await?
                {
                    return Ok(b);
                }
                let mut details = Vec::new();
                if let Some(uid) = spec.uid {
                    details.push(format!("uid {uid}"));
                }
                if let Some(g) = &spec.group {
                    details.push(format!("group {g}"));
                }
                if !spec.groups.is_empty() {
                    details.push(format!("groups {}", spec.groups.join(",")));
                }
                if let Some(s) = &spec.shell {
                    details.push(format!("shell {s}"));
                }
                if spec.system {
                    details.push("system".into());
                }
                if spec.password.is_some() {
                    details.push("password set".into());
                }
                Ok(Planned::change(Diff::Create {
                    summary: format!("create user {} ({})", spec.name, details.join(", ")),
                    preview: None,
                }))
            }
            (Ensure::Present, Some(user)) => {
                let changes = self.changes(cx, spec, &user).await?;
                if changes.summary.is_empty() {
                    return Ok(Planned::NoOp);
                }
                for (needed, tool) in [
                    (
                        !changes.usermod.is_empty() || !changes.add_groups.is_empty(),
                        "usermod",
                    ),
                    (changes.password, "chpasswd"),
                ] {
                    if needed
                        && let Some(b) =
                            hostkit::missing_tool(cx, tool, "install the `passwd` package").await?
                    {
                        return Ok(b);
                    }
                }
                Ok(Planned::change(Diff::Update {
                    summary: format!("update user {}: {}", spec.name, changes.summary.join(", ")),
                    unified: None,
                }))
            }
        }
    }

    #[instrument(name = "task.user.account.apply", skip_all, fields(user = %spec.name, state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        valid_name(&spec.name)?;
        let current = cx.system.lookup_user(&spec.name).await?;
        match (spec.state, current) {
            (Ensure::Absent, None) => Ok(Applied::unchanged(format!(
                "user {} was already absent",
                spec.name
            ))),
            (Ensure::Absent, Some(_)) => {
                hostkit::require_tool(cx, "userdel", "install the `passwd` package").await?;
                let mut cmd = Command::new("userdel");
                if spec.remove_home {
                    cmd = cmd.arg("-r");
                }
                hostkit::run_checked(cx, cmd.arg(&spec.name), "Deleting the user").await?;
                Ok(Applied::changed(format!("deleted user {}", spec.name)))
            }
            (Ensure::Present, None) => {
                hostkit::require_tool(cx, "useradd", "install the `passwd` package").await?;
                hostkit::run_checked(cx, self.useradd(spec), "Creating the user").await?;
                if let Some(password) = &spec.password {
                    self.set_password(cx, spec, password).await?;
                }
                let mut applied = Applied::changed(format!("created user {}", spec.name))
                    .with_artefact(Artefact::User {
                        name: spec.name.clone(),
                    });
                if spec.remove_home {
                    applied = applied.with_artefact(Artefact::Other {
                        description: REMOVE_HOME.into(),
                    });
                }
                Ok(applied)
            }
            (Ensure::Present, Some(user)) => {
                let changes = self.changes(cx, spec, &user).await?;
                if changes.summary.is_empty() {
                    return Ok(Applied::unchanged(format!(
                        "user {} already up to date",
                        spec.name
                    )));
                }
                if !changes.usermod.is_empty() || !changes.add_groups.is_empty() {
                    hostkit::require_tool(cx, "usermod", "install the `passwd` package").await?;
                    let mut cmd = Command::new("usermod").args(changes.usermod.clone());
                    if !changes.add_groups.is_empty() {
                        cmd = cmd.arg("-a").arg("-G").arg(changes.add_groups.join(","));
                    }
                    hostkit::run_checked(cx, cmd.arg(&spec.name), "Updating the user").await?;
                }
                if changes.password
                    && let Some(password) = &spec.password
                {
                    self.set_password(cx, spec, password).await?;
                }
                Ok(Applied::changed(format!(
                    "updated user {}: {}",
                    spec.name,
                    changes.summary.join(", ")
                )))
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let home = record
            .artefacts
            .iter()
            .any(|a| matches!(a, Artefact::Other { description } if description == REMOVE_HOME));
        format!(
            "delete user {}{} (only if Buckle created it)",
            record.key.key,
            if home { " and its home directory" } else { "" }
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(name) = record.artefacts.iter().find_map(|a| match a {
            Artefact::User { name } => Some(name.clone()),
            _ => None,
        }) else {
            return Ok(Applied::skipped(
                "the user existed before Buckle managed it, left in place",
            ));
        };
        if cx.system.lookup_user(&name).await?.is_none() {
            return Ok(Applied::unchanged(format!(
                "user {name} was already absent"
            )));
        }
        let remove_home = record
            .artefacts
            .iter()
            .any(|a| matches!(a, Artefact::Other { description } if description == REMOVE_HOME));
        hostkit::require_tool(cx, "userdel", "install the `passwd` package")
            .await
            .map_err(|e| {
                error::wrap_user(
                    e,
                    format!("Cannot delete user {name}."),
                    &["Install the passwd tools."],
                )
            })?;
        let mut cmd = Command::new("userdel");
        if remove_home {
            cmd = cmd.arg("-r");
        }
        hostkit::run_checked(cx, cmd.arg(&name), "Deleting the user").await?;
        Ok(Applied::changed(format!("deleted user {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::{FilePerms, GroupInfo};
    use buckle_core::testing::{FakeSystem, MemorySource};
    use rstest::rstest;
    use serde_json::json;

    fn user(name: &str, uid: u32) -> UserInfo {
        UserInfo {
            name: name.into(),
            uid,
            gid: 1000,
            home: format!("/home/{name}").into(),
            shell: "/bin/bash".into(),
            gecos: String::new(),
        }
    }

    fn group(name: &str, gid: u32, members: &[&str]) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            gid,
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"name": "alice"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    fn host() -> FakeSystem {
        with_account_tools(FakeSystem::new())
            .with_group(group("staff", 1000, &[]))
            .with_group(group("docker", 998, &[]))
            .with_group(group("sudo", 27, &["alice"]))
    }

    #[rstest]
    #[case::missing(host(), json!({}), Class::Change)]
    #[case::exists(host().with_user(user("alice", 1001)), json!({}), Class::NoOp)]
    #[case::uid_differs(host().with_user(user("alice", 1001)), json!({"uid": 1002}), Class::Change)]
    #[case::uid_matches(host().with_user(user("alice", 1001)), json!({"uid": 1001}), Class::NoOp)]
    #[case::shell_differs(host().with_user(user("alice", 1001)), json!({"shell": "/bin/zsh"}), Class::Change)]
    #[case::shell_matches(host().with_user(user("alice", 1001)), json!({"shell": "/bin/bash"}), Class::NoOp)]
    #[case::needs_a_group(host().with_user(user("alice", 1001)), json!({"groups": ["docker"]}), Class::Change)]
    #[case::already_in_group(host().with_user(user("alice", 1001)), json!({"groups": ["sudo"]}), Class::NoOp)]
    #[case::primary_group_counts_as_membership(host().with_user(user("alice", 1001)), json!({"groups": ["staff"]}), Class::NoOp)]
    #[case::primary_group_changes(host().with_user(user("alice", 1001)), json!({"group": "docker"}), Class::Change)]
    #[case::group_created_by_an_earlier_task(host().with_user(user("alice", 1001)), json!({"groups": ["not-yet"]}), Class::Change)]
    #[case::absent_present(host().with_user(user("alice", 1001)), json!({"state": "absent"}), Class::Change)]
    #[case::absent_missing(host(), json!({"state": "absent"}), Class::NoOp)]
    #[case::no_useradd(FakeSystem::new(), json!({}), Class::Blocked)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        assert_eq!(
            plan_class(&UserAccount, &cx(&sys), &s(extra)).await,
            expected
        );
    }

    #[tokio::test]
    async fn creating_an_account_passes_the_expected_flags_and_replans_as_noop() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(
            json!({"uid": 1500, "group": "staff", "groups": ["docker", "sudo"], "shell": "/bin/zsh", "comment": "Alice A"}),
        );
        let applied = apply(&UserAccount, &c, &spec).await;
        assert!(applied.changed);
        assert_eq!(
            applied.artefacts,
            vec![Artefact::User {
                name: "alice".into()
            }]
        );
        let line = sys.command_lines().remove(0);
        for expected in [
            "useradd",
            "-u 1500",
            "-g staff",
            "-G docker,sudo",
            "-s /bin/zsh",
            "-m",
            "alice",
        ] {
            assert!(line.contains(expected), "{expected} in {line}");
        }
        assert!(!line.contains(" -r"), "{line}");
        // The fake host now has the user, but group membership is a usermod matter in the fake.
        let sys2 = sys.clone().with_group(group("docker", 998, &["alice"]));
        assert_eq!(
            plan_class(&UserAccount, &cx(&sys2), &spec).await,
            Class::NoOp
        );
    }

    #[rstest]
    #[case::default_for_normal_accounts(json!({}), "-m")]
    #[case::explicit_no_home(json!({"create_home": false}), "-M")]
    #[case::system_accounts_default_to_no_home(json!({"system": true}), "-M")]
    #[case::system_with_home(json!({"system": true, "create_home": true}), "-m")]
    #[tokio::test]
    async fn create_home_flags(#[case] extra: serde_json::Value, #[case] flag: &str) {
        let sys = host();
        apply(&UserAccount, &cx(&sys), &s(extra.clone())).await;
        let args = sys.commands_run()[0].args.clone();
        assert!(args.contains(&flag.to_string()), "{args:?}");
        assert_eq!(
            args.contains(&"-r".to_string()),
            extra.get("system").is_some()
        );
    }

    #[tokio::test]
    async fn modifying_an_account_uses_usermod_with_only_the_changes() {
        let sys = host().with_user(user("alice", 1001));
        let c = cx(&sys);
        let spec = s(json!({"shell": "/bin/zsh", "groups": ["docker"]}));
        apply(&UserAccount, &c, &spec).await;
        let line = sys.command_lines().remove(0);
        assert!(
            line.starts_with("usermod -s /bin/zsh -a -G docker alice"),
            "{line}"
        );
        assert!(!sys.command_lines().iter().any(|l| l.starts_with("useradd")));
    }

    const HASH: &str = "$6$salt$verysecrethashvalue";

    #[tokio::test]
    async fn the_password_travels_on_stdin_never_on_a_command_line() {
        let sys = host();
        let c = cx_secrets(&sys, MemorySource::new(), secrets("pw", HASH));
        let spec = s(json!({"password": "{{ secrets.pw }}"}));
        let planned = UserAccount.plan(&c, &spec).await.unwrap();
        let applied = UserAccount.apply(&c, &spec, &planned).await.unwrap();

        let ran = sys.commands_run();
        let chpasswd = ran
            .iter()
            .find(|c| c.program == "chpasswd")
            .expect("chpasswd ran");
        assert_eq!(chpasswd.args, vec!["-e"]);
        assert_eq!(
            chpasswd.stdin.as_deref(),
            Some(format!("alice:{HASH}").as_bytes())
        );
        assert!(chpasswd.sensitive_env);
        for command in &ran {
            assert!(
                !command.display_line().contains(HASH),
                "argv leaked: {}",
                command.display_line()
            );
            assert!(
                !command.env.values().any(|v| v.contains(HASH)),
                "env leaked"
            );
        }
        assert!(!serialised(&planned, &applied).contains(HASH));
    }

    #[tokio::test]
    async fn literal_hashes_are_also_sent_on_stdin() {
        let sys = host();
        apply(&UserAccount, &cx(&sys), &s(json!({"password": HASH}))).await;
        let ran = sys.commands_run();
        assert!(ran.iter().all(|c| !c.display_line().contains(HASH)));
        assert!(
            ran.iter()
                .any(|c| c.program == "chpasswd" && c.stdin.is_some())
        );
    }

    #[tokio::test]
    async fn existing_accounts_are_compared_with_the_shadow_file_by_digest() {
        let shadow =
            |hash: &str| format!("root:*:19000:0:99999:7:::\nalice:{hash}:19000:0:99999:7:::\n");
        let spec = s(json!({"password": "{{ secrets.pw }}"}));

        let sys = host().with_user(user("alice", 1001)).with_file(
            "/etc/shadow",
            shadow(HASH),
            FilePerms::mode(0o640),
        );
        let c = cx_secrets(&sys, MemorySource::new(), secrets("pw", HASH));
        assert_eq!(plan_class(&UserAccount, &c, &spec).await, Class::NoOp);

        let sys = host().with_user(user("alice", 1001)).with_file(
            "/etc/shadow",
            shadow("!"),
            FilePerms::mode(0o640),
        );
        let c = cx_secrets(&sys, MemorySource::new(), secrets("pw", HASH));
        let planned = UserAccount.plan(&c, &spec).await.unwrap();
        assert!(diff(&planned).summary().contains("password changed"));
        assert!(!serde_json::to_string(&planned).unwrap().contains(HASH));
        UserAccount.apply(&c, &spec, &planned).await.unwrap();
        assert_eq!(sys.command_lines(), vec!["chpasswd -e"]);

        // No readable shadow: the password is assumed to be right.
        let sys = host().with_user(user("alice", 1001));
        let c = cx_secrets(&sys, MemorySource::new(), secrets("pw", HASH));
        assert_eq!(plan_class(&UserAccount, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn deleting_and_removal() {
        let sys = host().with_user(user("alice", 1001));
        let c = cx(&sys);
        apply(
            &UserAccount,
            &c,
            &s(json!({"state": "absent", "remove_home": true})),
        )
        .await;
        assert_eq!(sys.command_lines(), vec!["userdel -r alice"]);

        // Removal deletes only accounts Buckle created, and honours remove_home.
        let sys = host();
        let c = cx(&sys);
        let created = record(
            "user.account",
            "alice",
            &apply(&UserAccount, &c, &s(json!({"remove_home": true}))).await,
        );
        assert!(UserAccount::REMOVAL_CAUTION);
        assert!(
            UserAccount
                .describe_removal(&created)
                .contains("home directory")
        );
        assert!(UserAccount.remove(&c, &created).await.unwrap().changed);
        assert!(
            sys.command_lines()
                .contains(&"userdel -r alice".to_string())
        );

        let sys = host().with_user(user("bob", 1002));
        let c = cx(&sys);
        let rec = record("user.account", "bob", &Applied::unchanged("adopted"));
        let out = UserAccount.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("existed before"));
        assert!(sys.command_lines().is_empty());

        let sys = host();
        let rec = record(
            "user.account",
            "alice",
            &Applied::changed("x").with_artefact(Artefact::User {
                name: "alice".into(),
            }),
        );
        assert!(!UserAccount.remove(&cx(&sys), &rec).await.unwrap().changed);
    }

    #[test]
    fn dangerous_names_are_rejected() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for bad in ["", "a b", "x:y", "-r", "a;b"] {
            let result = rt.block_on(UserAccount.plan(&c, &s(json!({"name": bad}))));
            assert!(result.is_err(), "{bad}");
        }
    }
}
