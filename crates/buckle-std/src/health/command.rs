//! The `command` health check.

use async_trait::async_trait;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::system::Command;
use buckle_core::task::Cx;
use buckle_core::{Platform, Platforms};
use humantime_serde::re::humantime::format_duration;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Runs a command and checks its exit status and, optionally, its output.
///
/// `run` is either a list (an argv, run directly) or a string (run through
/// `sh -c`). The check passes when the exit status equals `expect.exit`
/// (default 0) and, if `expect.stdout_matches` is set, the regular expression
/// matches somewhere in standard output. A command that cannot be started
/// (missing program) is unknown rather than failed. `timeout` defaults to 30s
/// and a command that exceeds it fails.
///
/// Examples: `tailscale status`, or `rustic snapshots --json | jq -e 'length > 0'`.
pub struct CommandCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The command: an argv list, or a shell string.
    pub run: Run,
    /// What the command must do.
    #[serde(default)]
    pub expect: Expect,
    /// How long the command may run; defaults to 30s.
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Run {
    /// Run through the shell (`sh -c`).
    Shell(String),
    /// Run directly; the first element is the program.
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The required exit status; defaults to 0.
    #[serde(default)]
    pub exit: i32,
    /// A regular expression that standard output must match.
    #[serde(default)]
    pub stdout_matches: Option<String>,
}

fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

impl Run {
    fn command(&self, platform: Platform) -> Option<Command> {
        match self {
            Run::Argv(argv) => Command::from_argv(argv),
            Run::Shell(line) if line.trim().is_empty() => None,
            Run::Shell(line) if platform == Platform::Windows => {
                Some(Command::new("cmd").args(["/C", line.as_str()]))
            }
            Run::Shell(line) => Some(Command::new("sh").args(["-c", line.as_str()])),
        }
    }

    fn display(&self) -> String {
        match self {
            Run::Shell(s) => s.clone(),
            Run::Argv(a) => a.join(" "),
        }
    }
}

#[async_trait]
impl HealthCheck for CommandCheck {
    type Spec = Spec;
    const KIND: &'static str = "command";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Run a command and check its exit status and output.";

    #[tracing::instrument(name = "health.command", skip_all)]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        // Reasons can quote command output or response bodies: scrub known secrets.
        super::redacted(cx, self.probe(cx, spec).await)
    }
}

impl CommandCheck {
    async fn probe(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        let matcher = match spec
            .expect
            .stdout_matches
            .as_deref()
            .map(Regex::new)
            .transpose()
        {
            Ok(m) => m,
            Err(e) => {
                return HealthStatus::unknown(format!(
                    "`expect.stdout_matches` is not a valid regular expression: {e}"
                ));
            }
        };
        let Some(command) = spec.run.command(cx.platform()) else {
            return HealthStatus::unknown("`run` is empty");
        };
        let shown = spec.run.display();
        let output = match cx.system.run(command.timeout(spec.timeout)).await {
            Ok(o) => o,
            Err(e) => {
                return HealthStatus::unknown(format!(
                    "could not run `{shown}`: {}",
                    e.description()
                ));
            }
        };
        if output.timed_out {
            return HealthStatus::fail(format!(
                "`{shown}` did not finish within {}",
                format_duration(spec.timeout)
            ));
        }
        let Some(code) = output.code else {
            return HealthStatus::fail(format!("`{shown}` was terminated by a signal"));
        };
        if code != spec.expect.exit {
            let detail = output
                .stderr_str()
                .lines()
                .chain(output.stdout_str().lines())
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| format!(": {l}"))
                .unwrap_or_default();
            return HealthStatus::fail(format!(
                "`{shown}` exited {code} (expected {}){detail}",
                spec.expect.exit
            ));
        }
        if let Some(re) = matcher
            && !re.is_match(&output.stdout_str())
        {
            return HealthStatus::fail(format!(
                "output of `{shown}` does not match /{}/",
                re.as_str()
            ));
        }
        HealthStatus::Pass
    }
}

buckle_core::register_health!(CommandCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::system::Output;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn out(code: Option<i32>, stdout: &str, stderr: &str, timed_out: bool) -> Output {
        Output {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            timed_out,
        }
    }

    #[rstest]
    #[case::ok(out(Some(0), "100.64.0.1 web-1\n", "", false), json!({"run": ["tailscale", "status"]}), None)]
    #[case::expected_nonzero_exit(out(Some(3), "", "", false), json!({"run": ["tailscale", "status"], "expect": {"exit": 3}}), None)]
    #[case::stdout_matches(out(Some(0), "Snapshots: 14\n", "", false), json!({"run": ["tailscale", "status"], "expect": {"stdout_matches": "Snapshots: [1-9]"}}), None)]
    #[case::failed(out(Some(1), "", "Tailscale is stopped.\n", false), json!({"run": ["tailscale", "status"]}), Some("exited 1 (expected 0): Tailscale is stopped."))]
    #[case::wrong_exit(out(Some(0), "", "", false), json!({"run": ["tailscale", "status"], "expect": {"exit": 2}}), Some("exited 0 (expected 2)"))]
    #[case::output_mismatch(out(Some(0), "Snapshots: 0\n", "", false), json!({"run": ["tailscale", "status"], "expect": {"stdout_matches": "Snapshots: [1-9]"}}), Some("does not match /Snapshots: [1-9]/"))]
    #[case::timed_out(out(None, "", "", true), json!({"run": ["tailscale", "status"], "timeout": "2s"}), Some("did not finish within 2s"))]
    #[case::signalled(out(None, "", "", false), json!({"run": ["tailscale", "status"]}), Some("terminated by a signal"))]
    #[tokio::test]
    async fn exit_status_and_output(
        #[case] output: Output,
        #[case] spec_json: serde_json::Value,
        #[case] failure: Option<&str>,
    ) {
        let sys = FakeSystem::new().on_command("tailscale", &["status"], move |_| output.clone());
        let result = CommandCheck.check(&cx(&sys), &spec(spec_json)).await;
        match failure {
            None => assert_eq!(result, HealthStatus::Pass, "{result:?}"),
            Some(needle) => match result {
                HealthStatus::Fail { reason } => assert!(reason.contains(needle), "{reason}"),
                other => panic!("expected failure containing {needle:?}, got {other:?}"),
            },
        }
    }

    #[tokio::test]
    async fn shell_strings_run_through_sh_and_argv_runs_directly() {
        let sys = FakeSystem::new()
            .on_command_ok("sh", &["-c"], "ok\n")
            .on_command_ok("curl", &[], "ok\n");
        let shell: Spec = spec(json!({"run": "curl -s localhost | grep ok"}));
        assert_eq!(
            CommandCheck.check(&cx(&sys), &shell).await,
            HealthStatus::Pass
        );
        let argv: Spec = spec(json!({"run": ["curl", "-s", "localhost"], "timeout": "5s"}));
        assert_eq!(
            CommandCheck.check(&cx(&sys), &argv).await,
            HealthStatus::Pass
        );
        let ran = sys.commands_run();
        assert_eq!(
            ran[0].display_line(),
            "sh -c \"curl -s localhost | grep ok\""
        );
        assert_eq!(ran[1].display_line(), "curl -s localhost");
        assert_eq!(ran[1].timeout, Some(Duration::from_secs(5)));
        assert_eq!(ran[0].timeout, Some(Duration::from_secs(30)));
    }

    #[rstest]
    #[case::missing_program(json!({"run": ["nonexistent"]}), "could not run")]
    #[case::empty_argv(json!({"run": []}), "`run` is empty")]
    #[case::empty_string(json!({"run": "  "}), "`run` is empty")]
    #[case::bad_regex(json!({"run": ["x"], "expect": {"stdout_matches": "("}}), "not a valid regular expression")]
    #[tokio::test]
    async fn checks_that_cannot_run_are_unknown(
        #[case] spec_json: serde_json::Value,
        #[case] needle: &str,
    ) {
        let result = CommandCheck
            .check(&cx(&FakeSystem::new()), &spec(spec_json))
            .await;
        match result {
            HealthStatus::Unknown { reason } => assert!(reason.contains(needle), "{reason}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failure_reasons_are_scrubbed_of_secret_values() {
        let sys = FakeSystem::new().on_command_fail(
            "probe",
            &[],
            2,
            "connect: password=pa55-word-x rejected",
        );
        let c = crate::tasks::hostkit::testkit::cx_secrets(
            &sys,
            buckle_core::testing::MemorySource::new(),
            crate::tasks::hostkit::testkit::secrets("db", "pa55-word-x"),
        );
        let status = CommandCheck
            .check(&c, &spec(json!({"run": ["probe"]})))
            .await;
        let HealthStatus::Fail { reason } = status else {
            panic!("{status:?}")
        };
        assert!(
            !reason.contains("pa55-word-x") && reason.contains("secret:db@"),
            "{reason}"
        );
    }
}
