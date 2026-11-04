//! `command.run`: the escape hatch, kept plannable by idempotency guards.

use super::hostkit::{self, StringOrArgv};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::{Rendered, SecretField};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, Output};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::instrument;

/// Runs a command, guarded so that it only runs when something needs doing.
///
/// ```yaml
/// - command.run:
///     run: ["nomad", "node", "eligibility", "-enable", "-self"]   # argv, or a string run with `sh -c`
///     unless: "nomad node status -self -json | grep -q '\"SchedulingEligibility\": \"eligible\"'"
///     creates: /var/lib/app/initialised     # skip when this path exists
///     only_if: ["test", "-d", "/srv/data"]  # skip unless this exits 0
///     changed_when: "output_contains:created"   # always | never | output_contains:<text> | exit:<code>
///     env:
///       API_TOKEN: "{{ secrets.api_token }}"  # secrets go through the process-environment sink
///     cwd: /srv/app
///     user: deploy                           # run as this account (runuser on Linux, sudo -u on macOS)
///     timeout: 2m
///     remove: ["rm", "-f", "/var/lib/app/initialised"]   # cleanup when the task leaves the config
///     capture: redacted                      # redacted (default) | always | never
/// ```
///
/// **Planning.** `creates` existing, `unless` exiting 0 or `only_if` exiting
/// non-zero all make the plan a no-op. The `unless` and `only_if` probes run in
/// plan mode too, so they must be read-only. A command with none of `creates`,
/// `unless` or `changed_when` is planned as `always runs: <command>` so the
/// author sees the smell; the plan of such a task is never a no-op.
///
/// **Applying.** The command runs and fails the task on a non-zero exit (except
/// when `changed_when: exit:<code>` names that code). `changed_when` decides
/// whether the run counts as a change for handlers: `always` (the default),
/// `never`, `output_contains:<text>` (stdout or stderr) or `exit:<code>`.
///
/// **Secrets.** An `env` value that references `secrets.` is rendered with the
/// real value and handed to the process through the environment sink; it is
/// never rendered into the plan, the logs or the state. When any env value is
/// secret, the command's output is not captured (nor shown in errors) unless
/// `capture: always` is set. By default (`capture: redacted`) stdout and stderr
/// are recorded on the task's tracing span, truncated to 4 KiB each; `never`
/// records nothing.
///
/// **Running as a user.** With `user`, the command, its probes and its cleanup
/// run as that account: through `runuser -u USER --` on Linux (util-linux; the
/// step is blocked while it is missing) and `sudo -u USER` on macOS. The
/// account's `HOME`, `USER` and `SHELL` are set as a login would set them; the
/// task's `env` is passed through. Windows does not support it.
///
/// **Removal.** With `remove`, the cleanup command runs when the task leaves
/// the configuration (in the same `cwd`, as the same `user`, with the
/// non-secret `env`). Without it the removal is reported as "no cleanup
/// defined".
///
/// **Identity.** The resource is keyed by its rendered command, so editing the
/// command or a var inside it re-keys it (a removal plus an addition). Give a
/// templated command, or one with `remove`, a stable task `id`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The command: an argv list, or a string run with `sh -c`.
    pub run: StringOrArgv,
    /// Skip when this path exists.
    #[serde(default)]
    pub creates: Option<PathBuf>,
    /// A probe that exits 0 when nothing needs doing. Runs in plan mode too.
    #[serde(default)]
    pub unless: Option<StringOrArgv>,
    /// A probe that must exit 0 for the command to run.
    #[serde(default)]
    pub only_if: Option<StringOrArgv>,
    /// `always`, `never`, `output_contains:<text>` or `exit:<code>`.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub changed_when: Option<ChangedWhen>,
    /// Environment variables; values may be templates and may reference secrets
    /// (they are rendered for real and handed over through the environment sink).
    #[serde(default)]
    pub env: BTreeMap<String, SecretField>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Run the command, its probes and its cleanup as this user instead of the agent's account.
    #[serde(default)]
    pub user: Option<String>,
    /// How long the command may run, e.g. `30s` or `5m`.
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub timeout: Option<Duration>,
    /// A cleanup command run when the task is removed from the configuration.
    #[serde(default)]
    pub remove: Option<StringOrArgv>,
    /// Whether output is recorded on the task's span.
    #[serde(default)]
    pub capture: Capture,
}

/// How stdout and stderr are recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Capture {
    /// Record output unless the environment carried a secret.
    #[default]
    Redacted,
    /// Record output even if the environment carried a secret.
    Always,
    /// Record nothing.
    Never,
}

/// When a successful run counts as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedWhen {
    Always,
    Never,
    OutputContains(String),
    Exit(i32),
}

impl TryFrom<String> for ChangedWhen {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, String> {
        let value = value.trim();
        match value {
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => {
                if let Some(text) = value.strip_prefix("output_contains:") {
                    Ok(Self::OutputContains(text.to_owned()))
                } else if let Some(code) = value.strip_prefix("exit:") {
                    code.trim()
                        .parse()
                        .map(Self::Exit)
                        .map_err(|_| format!("'{code}' is not an exit code; use e.g. `exit:2`"))
                } else {
                    Err(format!(
                        "'{value}' is not a valid changed_when; use always, never, output_contains:<text> or exit:<code>"
                    ))
                }
            }
        }
    }
}

impl From<ChangedWhen> for String {
    fn from(value: ChangedWhen) -> Self {
        match value {
            ChangedWhen::Always => "always".into(),
            ChangedWhen::Never => "never".into(),
            ChangedWhen::OutputContains(t) => format!("output_contains:{t}"),
            ChangedWhen::Exit(c) => format!("exit:{c}"),
        }
    }
}

impl Serialize for ChangedWhen {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&String::from(self.clone()))
    }
}

impl<'de> Deserialize<'de> for ChangedWhen {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

pub struct CommandRun;
buckle_core::register_task!(CommandRun);

/// What removal needs to run the cleanup later; stored in the record's artefacts.
#[derive(Debug, Serialize, Deserialize)]
struct Cleanup {
    remove: StringOrArgv,
    cwd: Option<PathBuf>,
    #[serde(default)]
    user: Option<String>,
    /// Only the env values that were plain strings; secrets are never persisted.
    env: BTreeMap<String, String>,
}

const CLEANUP_PREFIX: &str = "command.run cleanup: ";
const OUTPUT_CAPTURE_LIMIT: usize = 4096;
const RUNUSER_HINT: &str =
    "it is part of util-linux and is needed to run a command as another user";

/// Wraps `command` so that it runs as `user`: `runuser -u USER -- …` on Linux,
/// `sudo -u USER …` on macOS. The wrapper keeps the cwd, timeout and
/// environment (secret-bearing values included) of the command it wraps.
fn as_user(platform: buckle_core::Platform, user: &str, command: Command) -> Result<Command> {
    super::user_group::valid_name(user)?;
    let mut wrapper = match platform {
        buckle_core::Platform::Linux => Command::new("runuser").args(["-u", user, "--"]),
        buckle_core::Platform::MacOs => Command::new("sudo").args(["-u", user]),
        buckle_core::Platform::Windows => {
            return Err(error::user(
                "`user` is not supported for command.run on Windows.",
                &[
                    "Remove the `user` field, or restrict the task with `when` to Linux and macOS hosts.",
                ],
            ));
        }
    };
    wrapper.args.push(command.program);
    wrapper.args.extend(command.args);
    wrapper.env = command.env;
    wrapper.clear_env = command.clear_env;
    wrapper.cwd = command.cwd;
    wrapper.stdin = command.stdin;
    wrapper.timeout = command.timeout;
    wrapper.sensitive_env = command.sensitive_env;
    Ok(wrapper)
}

/// The program `as_user` needs on this platform, if any.
fn user_wrapper_tool(platform: buckle_core::Platform) -> Option<&'static str> {
    match platform {
        buckle_core::Platform::Linux => Some("runuser"),
        buckle_core::Platform::MacOs => Some("sudo"),
        buckle_core::Platform::Windows => None,
    }
}

/// Whether the plan of this task is "always runs": no guard says when it is done.
pub fn always_runs(spec: &Spec) -> bool {
    spec.creates.is_none() && spec.unless.is_none() && spec.changed_when.is_none()
}

impl CommandRun {
    /// Builds the process for `argv` with the spec's cwd, timeout and environment,
    /// before any `user` wrapper. Secret env values are rendered for real and
    /// injected through the sink.
    fn build(&self, cx: &Cx, spec: &Spec, argv: &StringOrArgv) -> Result<Command> {
        let mut command = argv.to_command(cx.platform())?;
        command.cwd = spec.cwd.clone();
        command.timeout = spec.timeout;
        for (name, field) in &spec.env {
            field.inject_env(&mut command, name.clone(), &cx.templater, &cx.host)?;
        }
        Ok(command)
    }

    /// Applies the spec's `user` to a built command.
    fn wrap(&self, cx: &Cx, spec: &Spec, command: Command) -> Result<Command> {
        match &spec.user {
            Some(user) => as_user(cx.platform(), user, command),
            None => Ok(command),
        }
    }

    /// Evaluates `creates`, `unless` and `only_if`; `Some(reason)` means skip.
    async fn skip_reason(&self, cx: &Cx, spec: &Spec) -> Result<Option<String>> {
        if let Some(path) = &spec.creates
            && cx.system.stat(path).await?.is_some()
        {
            return Ok(Some(format!("{} exists", path.display())));
        }
        if let Some(unless) = &spec.unless {
            let out = self.probe(cx, spec, self.build(cx, spec, unless)?).await?;
            if out.success() {
                return Ok(Some(format!("`{}` succeeded", unless.display())));
            }
        }
        if let Some(only_if) = &spec.only_if {
            let out = self.probe(cx, spec, self.build(cx, spec, only_if)?).await?;
            if !out.success() {
                return Ok(Some(format!("`{}` did not succeed", only_if.display())));
            }
        }
        Ok(None)
    }

    /// Runs an `unless` / `only_if` probe ([`hostkit::probe_with`]), as the
    /// task's user.
    async fn probe(&self, cx: &Cx, spec: &Spec, command: Command) -> Result<Output> {
        hostkit::probe_with(cx, command, |c| self.wrap(cx, spec, c)).await
    }

    fn changed(spec: &Spec, out: &Output) -> bool {
        match &spec.changed_when {
            None | Some(ChangedWhen::Always) => true,
            Some(ChangedWhen::Never) => false,
            Some(ChangedWhen::OutputContains(text)) => {
                out.stdout_str().contains(text) || out.stderr_str().contains(text)
            }
            Some(ChangedWhen::Exit(code)) => out.code == Some(*code),
        }
    }

    /// Records the exit code and, when `capture` allows, the output on the
    /// span. Output always goes through the secret scrubber first: a command
    /// may print a secret it read from a file, not only from its environment.
    fn record_output(cx: &Cx, spec: &Spec, command: &Command, out: &Output) {
        let allowed = match spec.capture {
            Capture::Never => false,
            Capture::Redacted => !command.sensitive_env,
            Capture::Always => true,
        };
        let span = tracing::Span::current();
        if let Some(code) = out.code {
            span.record("exit_code", code);
        }
        if allowed {
            let redactor = buckle_core::secrets::redact::Redactor::new(&cx.secrets);
            span.record(
                "stdout",
                hostkit::clip(
                    redactor.redact_bytes(&out.stdout).get().trim(),
                    OUTPUT_CAPTURE_LIMIT,
                ),
            );
            span.record(
                "stderr",
                hostkit::clip(
                    redactor.redact_bytes(&out.stderr).get().trim(),
                    OUTPUT_CAPTURE_LIMIT,
                ),
            );
        }
    }
}

#[async_trait]
impl Task for CommandRun {
    type Spec = Spec;
    const KIND: &'static str = "command.run";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Run a command, made idempotent with creates, unless, only_if and changed_when; secrets reach it through the environment.";

    fn key(spec: &Spec) -> String {
        hostkit::clip(&spec.run.display(), 120)
    }

    fn always_runs(spec: &Spec) -> bool {
        always_runs(spec)
    }

    fn describe(spec: &Spec) -> String {
        if always_runs(spec) {
            format!("always runs: {}", hostkit::clip(&spec.run.display(), 120))
        } else {
            format!("run: {}", hostkit::clip(&spec.run.display(), 120))
        }
    }

    #[instrument(name = "task.command.run.plan", skip_all, fields(command = %hostkit::clip(&spec.run.display(), 200)))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        // Validate the command (and the user wrapper) up front so a typo is a
        // plan error, not an apply surprise.
        self.wrap(cx, spec, self.build(cx, spec, &spec.run)?)?;
        if spec.user.is_some()
            && let Some(tool) = user_wrapper_tool(cx.platform())
            && let Some(blocked) = hostkit::missing_tool(cx, tool, RUNUSER_HINT).await?
        {
            return Ok(blocked);
        }
        if self.skip_reason(cx, spec).await?.is_some() {
            return Ok(Planned::NoOp);
        }
        let summary = if always_runs(spec) {
            format!("always runs: {}", hostkit::clip(&spec.run.display(), 200))
        } else {
            format!("run: {}", hostkit::clip(&spec.run.display(), 200))
        };
        Ok(Planned::change(Diff::Opaque { summary }))
    }

    #[instrument(
        name = "task.command.run.apply",
        skip_all,
        fields(
            command = %hostkit::clip(&spec.run.display(), 200),
            exit_code = tracing::field::Empty,
            stdout = tracing::field::Empty,
            stderr = tracing::field::Empty,
        )
    )]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        if spec.user.is_some()
            && let Some(tool) = user_wrapper_tool(cx.platform())
        {
            hostkit::require_tool(cx, tool, RUNUSER_HINT).await?;
        }
        if let Some(reason) = self.skip_reason(cx, spec).await? {
            return Ok(Applied::unchanged(format!("skipped: {reason}")));
        }
        let command = self.wrap(cx, spec, self.build(cx, spec, &spec.run)?)?;
        let line = command.display_line();
        let sensitive = command.sensitive_env;
        let out = cx.system.run(command.clone()).await?;
        Self::record_output(cx, spec, &command, &out);
        let tolerated =
            matches!(spec.changed_when, Some(ChangedWhen::Exit(code)) if out.code == Some(code));
        if !out.success() && !tolerated {
            return Err(hostkit::command_failed(
                cx,
                "The command",
                &hostkit::clip(&line, 300),
                &out,
                sensitive && spec.capture != Capture::Always,
            ));
        }
        let code = out
            .code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let changed = Self::changed(spec, &out);
        let summary = format!("ran `{}` (exit {code})", hostkit::clip(&line, 200));
        let mut applied = if changed {
            Applied::changed(summary)
        } else {
            Applied::unchanged(summary)
        };
        if let Some(remove) = &spec.remove {
            let cleanup = Cleanup {
                remove: remove.clone(),
                cwd: spec.cwd.clone(),
                user: spec.user.clone(),
                env: spec
                    .env
                    .iter()
                    .filter(|(_, field)| !field.references_secrets())
                    .filter_map(|(k, field)| match field.resolve(&cx.templater, &cx.host) {
                        Ok(Rendered::Plain(text)) => Some((k.clone(), text)),
                        _ => None,
                    })
                    .collect(),
            };
            let json = serde_json::to_string(&cleanup).map_err(|e| {
                error::wrap_system(
                    e,
                    "Could not record the cleanup command.",
                    &["This is a bug in Buckle; please report it."],
                )
            })?;
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{CLEANUP_PREFIX}{json}"),
            });
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match cleanup_of(record) {
            Some(cleanup) => format!("run `{}`", hostkit::clip(&cleanup.remove.display(), 120)),
            None => format!("no cleanup defined for {}", record.key.key),
        }
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(cleanup) = cleanup_of(record) else {
            return Ok(Applied::skipped("no cleanup defined"));
        };
        let mut command = cleanup.remove.to_command(cx.platform())?;
        command.cwd = cleanup.cwd.clone();
        command.env.extend(cleanup.env.clone());
        if let Some(user) = &cleanup.user {
            if let Some(tool) = user_wrapper_tool(cx.platform()) {
                hostkit::require_tool(cx, tool, RUNUSER_HINT).await?;
            }
            command = as_user(cx.platform(), user, command)?;
        }
        let line = command.display_line();
        hostkit::run_checked(cx, command, "The cleanup command").await?;
        Ok(Applied::changed(format!(
            "ran cleanup `{}`",
            hostkit::clip(&line, 200)
        )))
    }
}

fn cleanup_of(record: &ResourceRecord) -> Option<Cleanup> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Other { description } => description
            .strip_prefix(CLEANUP_PREFIX)
            .and_then(|json| serde_json::from_str(json).ok()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::{FakeSystem, MemorySource, ok_output};
    use rstest::rstest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"run": ["do-thing", "--now"]});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    fn fail(code: i32) -> impl Fn(&Command) -> Output + Send + Sync + 'static {
        move |_| Output {
            code: Some(code),
            stdout: vec![],
            stderr: b"nope".to_vec(),
            timed_out: false,
        }
    }

    /// (guard spec, initial host, expected plan class)
    #[rstest]
    #[case::unguarded_always_runs(json!({}), FakeSystem::new(), Class::Change)]
    #[case::creates_missing(json!({"creates": "/var/done"}), FakeSystem::new(), Class::Change)]
    #[case::creates_exists(json!({"creates": "/var/done"}), FakeSystem::new().with_file("/var/done", "", FilePerms::default()), Class::NoOp)]
    #[case::unless_succeeds(json!({"unless": ["probe"]}), FakeSystem::new().on_command_ok("probe", &[], ""), Class::NoOp)]
    #[case::unless_fails(json!({"unless": ["probe"]}), FakeSystem::new().on_command("probe", &[], fail(1)), Class::Change)]
    #[case::only_if_succeeds(json!({"only_if": ["cond"]}), FakeSystem::new().on_command_ok("cond", &[], ""), Class::Change)]
    #[case::only_if_fails(json!({"only_if": ["cond"]}), FakeSystem::new().on_command("cond", &[], fail(1)), Class::NoOp)]
    #[case::unless_string_runs_in_a_shell(json!({"unless": "test -f /x"}), FakeSystem::new().on_command_ok("sh", &["-c", "test -f /x"], ""), Class::NoOp)]
    #[case::creates_wins_before_probes_run(json!({"creates": "/var/done", "unless": ["unscripted"]}), FakeSystem::new().with_file("/var/done", "", FilePerms::default()), Class::NoOp)]
    #[case::only_if_blocks_even_when_unless_fails(
        json!({"unless": ["probe"], "only_if": ["cond"]}),
        FakeSystem::new().on_command("probe", &[], fail(1)).on_command("cond", &[], fail(1)),
        Class::NoOp
    )]
    #[tokio::test]
    async fn plan_matrix(
        #[case] guards: serde_json::Value,
        #[case] sys: FakeSystem,
        #[case] expected: Class,
    ) {
        for mode in [
            buckle_core::task::Mode::Plan,
            buckle_core::task::Mode::Apply,
        ] {
            let c = buckle_core::testing::CxBuilder::new(sys.clone())
                .mode(mode)
                .build();
            assert_eq!(
                plan_class(&CommandRun, &c, &s(guards.clone())).await,
                expected,
                "{mode:?}"
            );
        }
        assert!(
            sys.command_lines()
                .iter()
                .all(|l| !l.starts_with("do-thing")),
            "plan never runs the command"
        );
    }

    #[tokio::test]
    async fn unguarded_commands_are_flagged_as_always_running() {
        let sys = FakeSystem::new();
        let planned = CommandRun.plan(&cx(&sys), &s(json!({}))).await.unwrap();
        assert!(
            diff(&planned)
                .summary()
                .starts_with("always runs: do-thing --now"),
            "{planned:?}"
        );
        assert!(always_runs(&s(json!({}))));
        assert!(!always_runs(&s(json!({"creates": "/x"}))));
        assert!(!always_runs(&s(json!({"changed_when": "never"}))));
        let guarded = CommandRun
            .plan(&cx(&sys), &s(json!({"creates": "/x"})))
            .await
            .unwrap();
        assert!(!diff(&guarded).summary().starts_with("always runs"));
    }

    #[rstest]
    #[case::default_is_changed(json!({}), "hello", 0, true)]
    #[case::always(json!({"changed_when": "always"}), "hello", 0, true)]
    #[case::never(json!({"changed_when": "never"}), "hello", 0, false)]
    #[case::output_contains_hit(json!({"changed_when": "output_contains:created 3"}), "created 3 rows", 0, true)]
    #[case::output_contains_miss(json!({"changed_when": "output_contains:created"}), "nothing to do", 0, false)]
    #[case::exit_code_hit(json!({"changed_when": "exit:2"}), "", 2, true)]
    #[case::exit_code_zero_is_unchanged(json!({"changed_when": "exit:2"}), "", 0, false)]
    #[tokio::test]
    async fn changed_when_decides_the_result(
        #[case] extra: serde_json::Value,
        #[case] stdout: &str,
        #[case] code: i32,
        #[case] changed: bool,
    ) {
        let out = stdout.to_owned();
        let sys = FakeSystem::new().on_command("do-thing", &[], move |_| Output {
            code: Some(code),
            stdout: out.clone().into_bytes(),
            stderr: vec![],
            timed_out: false,
        });
        let applied = CommandRun
            .apply(&cx(&sys), &s(extra), &Planned::NoOp)
            .await
            .unwrap();
        assert_eq!(applied.changed, changed, "{}", applied.summary);
    }

    #[tokio::test]
    async fn failures_are_user_errors_with_stderr() {
        let sys = FakeSystem::new().on_command("do-thing", &[], fail(3));
        let err = CommandRun
            .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        assert!(
            err.description().contains("status 3") && err.description().contains("nope"),
            "{}",
            err.description()
        );
    }

    #[tokio::test]
    async fn guards_are_rechecked_at_apply_time() {
        let sys = FakeSystem::new()
            .with_file("/var/done", "", FilePerms::default())
            .on_command_ok("do-thing", &[], "");
        let applied = CommandRun
            .apply(
                &cx(&sys),
                &s(json!({"creates": "/var/done"})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert!(!applied.changed && applied.summary.starts_with("skipped"));
        assert!(sys.command_lines().is_empty());
    }

    #[tokio::test]
    async fn creates_makes_the_second_plan_a_noop() {
        let sys = FakeSystem::new();
        let writer = sys.clone();
        let sys = sys.on_command("do-thing", &[], move |_| {
            // The command's side effect, as seen by `creates`.
            // Clones share state, so this is the command's side effect on the host.
            let _ = writer
                .clone()
                .with_file("/var/done", "", FilePerms::default());
            ok_output("done")
        });
        let c = cx(&sys);
        let spec = s(json!({"creates": "/var/done"}));
        apply(&CommandRun, &c, &spec).await;
        assert_eq!(plan_class(&CommandRun, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn shell_strings_run_through_sh_with_cwd_env_and_timeout() {
        let sys = FakeSystem::new().on_command_ok("sh", &["-c"], "");
        let spec = s(
            json!({"run": "echo $GREETING", "env": {"GREETING": "hi"}, "cwd": "/srv", "timeout": "30s"}),
        );
        apply(&CommandRun, &cx(&sys), &spec).await;
        let ran = &sys.commands_run()[0];
        assert_eq!(
            (ran.program.as_str(), ran.args.as_slice()),
            ("sh", &["-c".to_string(), "echo $GREETING".to_string()][..])
        );
        assert_eq!(ran.env.get("GREETING").map(String::as_str), Some("hi"));
        assert_eq!(ran.cwd.as_deref(), Some(std::path::Path::new("/srv")));
        assert_eq!(ran.timeout, Some(Duration::from_secs(30)));
    }

    fn secret_cx(sys: &FakeSystem) -> Cx {
        cx_secrets(sys, MemorySource::new(), secrets("api", "s3cr3t-value"))
    }

    #[tokio::test]
    async fn secret_env_values_reach_the_process_but_not_plans_results_or_args() {
        const SECRET: &str = "s3cr3t-value";
        let sys = FakeSystem::new().on_command("do-thing", &[], |_| Output {
            code: Some(0),
            stdout: format!("echoed {SECRET}").into_bytes(),
            stderr: vec![],
            timed_out: false,
        });
        let c = secret_cx(&sys);
        let spec = s(
            json!({"env": {"API_TOKEN": "{{ secrets.api }}", "MODE": "fast"}, "remove": ["undo"]}),
        );
        let planned = CommandRun.plan(&c, &spec).await.unwrap();
        let applied = CommandRun.apply(&c, &spec, &planned).await.unwrap();

        let ran = &sys.commands_run()[0];
        assert_eq!(ran.env.get("API_TOKEN").map(String::as_str), Some(SECRET));
        assert!(ran.sensitive_env);
        assert!(!ran.display_line().contains(SECRET) && !format!("{ran:?}").contains(SECRET));
        let everything = serialised(&planned, &applied);
        assert!(!everything.contains(SECRET), "{everything}");
        // The persisted cleanup never carries env values that were secret.
        assert!(
            applied
                .artefacts
                .iter()
                .all(|a| !format!("{a:?}").contains(SECRET)
                    && !format!("{a:?}").contains("API_TOKEN"))
        );
    }

    #[tokio::test]
    async fn failing_commands_with_secret_env_do_not_echo_their_output() {
        let sys = FakeSystem::new().on_command("do-thing", &[], |_| Output {
            code: Some(1),
            stdout: b"token=s3cr3t-value".to_vec(),
            stderr: b"bad s3cr3t-value".to_vec(),
            timed_out: false,
        });
        let spec = s(json!({"env": {"API_TOKEN": "{{ secrets.api }}"}}));
        let err = CommandRun
            .apply(&secret_cx(&sys), &spec, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(!err.message().contains("s3cr3t-value"), "{}", err.message());
    }

    #[tokio::test]
    async fn plain_templated_env_values_are_rendered_without_taint() {
        use buckle_core::selectors::{HostContext, HostIdentity, Vars};
        let mut vars = Vars::new();
        vars.insert("region".into(), json!("eu"));
        let host = HostContext::new(HostIdentity {
            id: "h".into(),
            ..Default::default()
        })
        .with_vars(vars);
        let sys = FakeSystem::new().on_command_ok("do-thing", &[], "");
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .host(host)
            .build();
        apply(
            &CommandRun,
            &c,
            &s(json!({"env": {"REGION": "{{ vars.region }}"}})),
        )
        .await;
        let ran = &sys.commands_run()[0];
        assert_eq!(ran.env.get("REGION").map(String::as_str), Some("eu"));
        assert!(!ran.sensitive_env);
    }

    #[tokio::test]
    async fn removal_runs_the_cleanup_or_reports_none_defined() {
        let sys = FakeSystem::new()
            .on_command_ok("do-thing", &[], "")
            .on_command_ok("undo", &[], "");
        let c = cx(&sys);
        let with = s(json!({"remove": ["undo", "--all"], "cwd": "/srv", "env": {"MODE": "fast"}}));
        let rec = record("command.run", "k", &apply(&CommandRun, &c, &with).await);
        assert_eq!(CommandRun.describe_removal(&rec), "run `undo --all`");
        let out = CommandRun.remove(&c, &rec).await.unwrap();
        assert!(out.changed);
        let undo = sys
            .commands_run()
            .into_iter()
            .find(|c| c.program == "undo")
            .unwrap();
        assert_eq!(undo.cwd.as_deref(), Some(std::path::Path::new("/srv")));
        assert_eq!(undo.env.get("MODE").map(String::as_str), Some("fast"));

        let without = s(json!({}));
        let rec = record("command.run", "k", &apply(&CommandRun, &c, &without).await);
        let out = CommandRun.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("no cleanup defined"));
        assert!(
            CommandRun
                .describe_removal(&rec)
                .contains("no cleanup defined")
        );
    }

    #[tokio::test]
    async fn user_wraps_the_command_its_probes_and_its_cleanup_on_linux() {
        let sys = FakeSystem::new()
            .with_program("runuser")
            .with_program("probe")
            .on_command("runuser", &["-u", "pi", "--", "probe"], fail(1))
            .on_command_ok("runuser", &["-u", "pi", "--", "do-thing"], "")
            .on_command_ok("runuser", &["-u", "pi", "--", "undo"], "");
        let c = cx(&sys);
        let spec = s(json!({
            "user": "pi", "unless": ["probe"], "cwd": "/home/pi", "env": {"MODE": "fast"}, "remove": ["undo"]
        }));
        assert_eq!(plan_class(&CommandRun, &c, &spec).await, Class::Change);
        let rec = record("command.run", "k", &apply(&CommandRun, &c, &spec).await);
        CommandRun.remove(&c, &rec).await.unwrap();

        let lines = sys.command_lines();
        assert!(
            lines.iter().all(|l| l.starts_with("runuser -u pi -- ")),
            "every process is wrapped: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.ends_with("probe")));
        assert!(lines.iter().any(|l| l.ends_with("do-thing --now")));
        assert!(lines.iter().any(|l| l.ends_with("undo")));
        let ran = sys
            .commands_run()
            .into_iter()
            .find(|c| c.args.last().map(String::as_str) == Some("--now"))
            .unwrap();
        assert_eq!(ran.cwd.as_deref(), Some(std::path::Path::new("/home/pi")));
        assert_eq!(ran.env.get("MODE").map(String::as_str), Some("fast"));
    }

    #[tokio::test]
    async fn user_is_blocked_without_runuser_and_uses_sudo_on_macos() {
        let sys = FakeSystem::new();
        let spec = s(json!({"user": "pi"}));
        assert_eq!(
            plan_class(&CommandRun, &cx(&sys), &spec).await,
            Class::Blocked
        );
        assert!(
            CommandRun
                .apply(&cx(&sys), &spec, &Planned::NoOp)
                .await
                .is_err()
        );

        let mac = FakeSystem::new()
            .with_platform(buckle_core::Platform::MacOs)
            .with_program("sudo")
            .on_command_ok("sudo", &["-u", "pi", "do-thing"], "");
        apply(&CommandRun, &cx(&mac), &spec).await;
        assert_eq!(mac.command_lines(), vec!["sudo -u pi do-thing --now"]);
    }

    #[rstest]
    #[case::windows(buckle_core::Platform::Windows, "pi")]
    #[case::bad_name(buckle_core::Platform::Linux, "-rf")]
    #[tokio::test]
    async fn unusable_users_are_user_errors(
        #[case] platform: buckle_core::Platform,
        #[case] user: &str,
    ) {
        let sys = FakeSystem::new()
            .with_platform(platform)
            .with_program("runuser");
        let err = CommandRun
            .plan(&cx(&sys), &s(json!({"user": user})))
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User), "{err:?}");
    }

    #[rstest]
    #[case::bad_changed_when(json!({"run": ["x"], "changed_when": "sometimes"}))]
    #[case::bad_exit(json!({"run": ["x"], "changed_when": "exit:abc"}))]
    #[case::bad_timeout(json!({"run": ["x"], "timeout": "soon"}))]
    fn invalid_specs_fail_to_parse(#[case] raw: serde_json::Value) {
        assert!(serde_json::from_value::<Spec>(raw).is_err());
    }

    #[tokio::test]
    async fn empty_commands_are_rejected_at_plan_time() {
        let sys = FakeSystem::new();
        assert!(
            CommandRun
                .plan(&cx(&sys), &s(json!({"run": []})))
                .await
                .is_err()
        );
        assert!(
            CommandRun
                .plan(&cx(&sys), &s(json!({"run": "  "})))
                .await
                .is_err()
        );
    }

    /// Captures span fields recorded by the tasks.
    #[derive(Clone, Default)]
    struct FieldLog(Arc<Mutex<Vec<(String, String)>>>);

    struct Visit<'a>(&'a FieldLog);
    impl tracing::field::Visit for Visit<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0
                .0
                .lock()
                .unwrap()
                .push((field.name().to_owned(), value.to_owned()));
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .0
                .lock()
                .unwrap()
                .push((field.name().to_owned(), format!("{value:?}")));
        }
    }

    impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
        tracing_subscriber::Layer<S> for FieldLog
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            attrs.record(&mut Visit(self));
        }
        fn on_record(
            &self,
            _id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            values.record(&mut Visit(self));
        }
    }

    async fn captured(spec: Spec, sys: FakeSystem, c: Cx) -> Vec<(String, String)> {
        use tracing_subscriber::layer::SubscriberExt;
        let log = FieldLog::default();
        let subscriber = tracing_subscriber::registry().with(log.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = sys;
        CommandRun.apply(&c, &spec, &Planned::NoOp).await.unwrap();
        log.0.lock().unwrap().clone()
    }

    #[rstest]
    #[case::redacted_captures_plain_runs(json!({}), false, true)]
    #[case::never_captures_nothing(json!({"capture": "never"}), false, false)]
    #[case::redacted_skips_secret_runs(json!({"env": {"T": "{{ secrets.api }}"}}), true, false)]
    #[case::always_captures_secret_runs(json!({"env": {"T": "{{ secrets.api }}"}, "capture": "always"}), true, true)]
    #[tokio::test]
    async fn output_capture_follows_the_policy(
        #[case] extra: serde_json::Value,
        #[case] with_secrets: bool,
        #[case] expect_output: bool,
    ) {
        let sys = FakeSystem::new().on_command_ok("do-thing", &[], "visible output");
        let c = if with_secrets {
            secret_cx(&sys)
        } else {
            cx(&sys)
        };
        let fields = captured(s(extra), sys, c).await;
        let stdout = fields
            .iter()
            .find(|(k, _)| k == "stdout")
            .map(|(_, v)| v.clone());
        assert_eq!(
            stdout.as_deref() == Some("visible output"),
            expect_output,
            "{fields:?}"
        );
        assert!(fields.iter().any(|(k, v)| k == "exit_code" && v == "0"));
    }

    #[tokio::test]
    async fn failure_messages_never_quote_a_known_secret() {
        // The command reads the secret itself (not via env) and prints it.
        let sys = FakeSystem::new().on_command_fail(
            "deploy",
            &[],
            1,
            "auth failed for token tok-123-secret",
        );
        let c = crate::tasks::hostkit::testkit::cx_secrets(
            &sys,
            buckle_core::testing::MemorySource::new(),
            crate::tasks::hostkit::testkit::secrets("api", "tok-123-secret"),
        );
        let s: Spec =
            serde_json::from_value(json!({"run": ["deploy"], "capture": "always"})).unwrap();
        let err = CommandRun.apply(&c, &s, &Planned::NoOp).await.unwrap_err();
        let text = format!("{} {:?}", err.description(), err);
        assert!(!text.contains("tok-123-secret"), "{text}");
        assert!(text.contains("secret:api@"), "{text}");
    }

    #[tokio::test]
    async fn a_probe_whose_program_is_missing_exits_127_instead_of_failing_the_plan() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let unless: Spec = serde_json::from_value(
            json!({"run": ["install-nomad"], "unless": ["nomad", "version"]}),
        )
        .unwrap();
        assert_eq!(
            plan_class(&CommandRun, &c, &unless).await,
            Class::Change,
            "not installed yet: run"
        );
        let only_if: Spec =
            serde_json::from_value(json!({"run": ["x"], "only_if": ["/usr/local/bin/missing"]}))
                .unwrap();
        assert_eq!(
            plan_class(&CommandRun, &c, &only_if).await,
            Class::NoOp,
            "127 is not success"
        );
    }
}
