//! The `systemd` health check.

use async_trait::async_trait;
use buckle_core::Platforms;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::system::Command;
use buckle_core::task::Cx;
use chrono::{DateTime, NaiveDateTime, Utc};
use humantime_serde::re::humantime::format_duration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Checks that a systemd unit is `active` (or `inactive`) and has stayed that way.
///
/// `state` is `active` (default) or `inactive`. For an active unit,
/// `min_uptime` additionally requires that it has been up at least that long
/// (from its `ActiveEnterTimestamp`), which catches crash loops: a unit that
/// restarted 4 seconds ago fails a `min_uptime: 30s` check with "restarted 4s
/// ago". `min_uptime` is ignored for `state: inactive`. A unit that systemd
/// does not know fails an `active` check. Needs systemd 247 or newer for
/// timestamps in UTC; older versions are accepted when the host's time zone is
/// UTC. Unknown when `systemctl` is missing.
pub struct SystemdCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The unit, e.g. `nomad.service`.
    pub unit: String,
    /// The state the unit must be in: `active` (default) or `inactive`.
    #[serde(default)]
    pub state: State,
    /// The unit must have been active at least this long.
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub min_uptime: Option<Duration>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Active,
    Inactive,
}

/// Parses `ActiveEnterTimestamp=Mon 2026-01-05 10:00:00 UTC`.
pub(crate) fn parse_timestamp(value: &str) -> Result<Option<DateTime<Utc>>, String> {
    let value = value.trim();
    if value.is_empty() || value == "n/a" {
        return Ok(None);
    }
    let mut parts = value.split_whitespace();
    // "Mon 2026-01-05 10:00:00 UTC" (the weekday is optional in some locales' output).
    let mut tokens: Vec<&str> = parts.by_ref().collect();
    if tokens
        .first()
        .is_some_and(|t| t.chars().all(|c| c.is_ascii_alphabetic()))
    {
        tokens.remove(0);
    }
    let [date, time, rest @ ..] = tokens.as_slice() else {
        return Err(format!("unrecognised timestamp '{value}'"));
    };
    let tz = rest.first().copied().unwrap_or("UTC");
    if !matches!(tz, "UTC" | "GMT" | "Z" | "+00" | "+0000" | "+00:00") {
        return Err(format!("timestamp '{value}' is not in UTC"));
    }
    let time = time.split('.').next().unwrap_or(time);
    NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S")
        .map(|n| Some(n.and_utc()))
        .map_err(|_| format!("unrecognised timestamp '{value}'"))
}

#[async_trait]
impl HealthCheck for SystemdCheck {
    type Spec = Spec;
    const KIND: &'static str = "systemd";
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Check that a systemd unit is active (or inactive), and has been up for min_uptime.";

    #[tracing::instrument(name = "health.systemd", skip_all, fields(unit = %spec.unit))]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        // Reasons can quote command output or response bodies: scrub known secrets.
        super::redacted(cx, self.probe(cx, spec).await)
    }
}

impl SystemdCheck {
    async fn probe(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        match cx.system.which("systemctl").await {
            Ok(Some(_)) => {}
            Ok(None) => return HealthStatus::unknown("systemctl is not installed on this host"),
            Err(e) => return HealthStatus::unknown(e.description()),
        }
        let props = "LoadState,ActiveState,SubState,ActiveEnterTimestamp";
        let mut stdout = None;
        let mut last_error = String::new();
        // --timestamp=utc needs systemd 247; retry without it on older hosts.
        for utc in [true, false] {
            let mut cmd = Command::new("systemctl").arg("show");
            if utc {
                cmd = cmd.arg("--timestamp=utc");
            }
            let cmd = cmd.args(["-p", props, spec.unit.as_str()]);
            match cx.system.run(cmd).await {
                Ok(out) if out.success() => {
                    stdout = Some(out.stdout_str());
                    break;
                }
                Ok(out) => last_error = out.stderr_str().trim().to_owned(),
                Err(e) => last_error = e.description(),
            }
        }
        let Some(stdout) = stdout else {
            return HealthStatus::unknown(format!(
                "`systemctl show {}` failed: {last_error}",
                spec.unit
            ));
        };
        let props: HashMap<&str, &str> = stdout
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let get = |k: &str| props.get(k).copied().unwrap_or("");
        let (load, active, sub) = (get("LoadState"), get("ActiveState"), get("SubState"));

        match spec.state {
            State::Inactive => {
                if active == "inactive" {
                    HealthStatus::Pass
                } else {
                    HealthStatus::fail(format!(
                        "{} is {active} ({sub}), expected inactive",
                        spec.unit
                    ))
                }
            }
            State::Active => {
                if load == "not-found" {
                    return HealthStatus::fail(format!("{} was not found by systemd", spec.unit));
                }
                if !matches!(active, "active" | "reloading") {
                    return HealthStatus::fail(format!(
                        "{} is {active} ({sub}), expected active",
                        spec.unit
                    ));
                }
                let Some(min) = spec.min_uptime else {
                    return HealthStatus::Pass;
                };
                let entered = match parse_timestamp(get("ActiveEnterTimestamp")) {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        return HealthStatus::unknown(format!(
                            "{} reports no ActiveEnterTimestamp, so its uptime is unknown",
                            spec.unit
                        ));
                    }
                    Err(e) => return HealthStatus::unknown(e),
                };
                let age = cx.system.now() - entered;
                let Ok(age) = age.to_std() else {
                    return HealthStatus::unknown(format!(
                        "{} appears to have started in the future (clock skew?)",
                        spec.unit
                    ));
                };
                if age < min {
                    HealthStatus::fail(format!(
                        "restarted {}s ago (min_uptime {})",
                        age.as_secs(),
                        format_duration(min)
                    ))
                } else {
                    HealthStatus::Pass
                }
            }
        }
    }
}

buckle_core::register_health!(SystemdCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    /// "Now" for these tests: `FakeSystem`'s clock is set to this instant.
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-05T10:00:30Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn show(load: &str, active: &str, sub: &str, entered: &str) -> String {
        format!(
            "LoadState={load}\nActiveState={active}\nSubState={sub}\nActiveEnterTimestamp={entered}\n"
        )
    }

    fn host(output: &str) -> FakeSystem {
        FakeSystem::new()
            .with_now(now())
            .on_command_ok("systemctl", &["show"], output)
    }

    #[rstest]
    #[case::active(show("loaded", "active", "running", "Mon 2026-01-05 09:00:00 UTC"), json!({"unit": "nomad.service"}), None)]
    #[case::reloading_counts_as_active(show("loaded", "reloading", "reload", "Mon 2026-01-05 09:00:00 UTC"), json!({"unit": "nomad"}), None)]
    #[case::inactive_expected(show("loaded", "inactive", "dead", ""), json!({"unit": "nomad", "state": "inactive"}), None)]
    #[case::failed(show("loaded", "failed", "failed", ""), json!({"unit": "nomad"}), Some("failed (failed), expected active"))]
    #[case::crash_looping(show("loaded", "activating", "auto-restart", ""), json!({"unit": "nomad"}), Some("activating (auto-restart)"))]
    #[case::not_found(show("not-found", "inactive", "dead", ""), json!({"unit": "ghost"}), Some("was not found"))]
    #[case::active_but_should_be_inactive(show("loaded", "active", "running", "Mon 2026-01-05 09:00:00 UTC"), json!({"unit": "nomad", "state": "inactive"}), Some("expected inactive"))]
    #[case::up_long_enough(show("loaded", "active", "running", "Mon 2026-01-05 09:59:00 UTC"), json!({"unit": "nomad", "min_uptime": "30s"}), None)]
    #[case::just_restarted(show("loaded", "active", "running", "Mon 2026-01-05 10:00:10 UTC"), json!({"unit": "nomad", "min_uptime": "30s"}), Some("restarted 20s ago (min_uptime 30s)"))]
    #[case::exactly_min_uptime(show("loaded", "active", "running", "Mon 2026-01-05 10:00:00 UTC"), json!({"unit": "nomad", "min_uptime": "30s"}), None)]
    #[tokio::test]
    async fn state_and_uptime(
        #[case] output: String,
        #[case] spec_json: serde_json::Value,
        #[case] failure: Option<&str>,
    ) {
        let result = SystemdCheck
            .check(&cx(&host(&output)), &spec(spec_json))
            .await;
        match failure {
            None => assert_eq!(result, HealthStatus::Pass, "{result:?}"),
            Some(needle) => match result {
                HealthStatus::Fail { reason } => assert!(reason.contains(needle), "{reason}"),
                other => panic!("expected failure containing {needle:?}, got {other:?}"),
            },
        }
    }

    #[tokio::test]
    async fn restarted_n_seconds_ago_uses_the_fixed_clock() {
        let sys = host(&show(
            "loaded",
            "active",
            "running",
            "Mon 2026-01-05 10:00:26 UTC",
        ));
        let result = SystemdCheck
            .check(
                &cx(&sys),
                &spec(json!({"unit": "nomad", "min_uptime": "30s"})),
            )
            .await;
        assert_eq!(
            result,
            HealthStatus::fail("restarted 4s ago (min_uptime 30s)")
        );
    }

    #[rstest]
    #[case::no_timestamp(show("loaded", "active", "running", ""), "no ActiveEnterTimestamp")]
    #[case::local_zone(
        show("loaded", "active", "running", "Mon 2026-01-05 11:00:00 CET"),
        "not in UTC"
    )]
    #[case::garbage(
        show("loaded", "active", "running", "yesterday"),
        "unrecognised timestamp"
    )]
    #[case::future(
        show("loaded", "active", "running", "Mon 2026-01-05 10:05:00 UTC"),
        "future"
    )]
    #[tokio::test]
    async fn uptime_that_cannot_be_determined_is_unknown(
        #[case] output: String,
        #[case] needle: &str,
    ) {
        let result = SystemdCheck
            .check(
                &cx(&host(&output)),
                &spec(json!({"unit": "nomad", "min_uptime": "30s"})),
            )
            .await;
        match result {
            HealthStatus::Unknown { reason } => assert!(reason.contains(needle), "{reason}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_without_systemctl_or_when_it_errors() {
        let no_systemctl = SystemdCheck
            .check(&cx(&FakeSystem::new()), &spec(json!({"unit": "nomad"})))
            .await;
        assert!(matches!(no_systemctl, HealthStatus::Unknown { .. }));
        let sys = FakeSystem::new().on_command_fail(
            "systemctl",
            &["show"],
            1,
            "Failed to connect to bus",
        );
        match SystemdCheck
            .check(&cx(&sys), &spec(json!({"unit": "nomad"})))
            .await
        {
            HealthStatus::Unknown { reason } => {
                assert!(reason.contains("Failed to connect to bus"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn older_systemd_without_timestamp_flag_is_retried() {
        let sys = FakeSystem::new()
            .with_now(now())
            .on_command("systemctl", &["show"], |cmd| {
                if cmd.args.iter().any(|a| a.starts_with("--timestamp")) {
                    buckle_core::system::Output {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"systemctl: unrecognized option '--timestamp=utc'".to_vec(),
                        timed_out: false,
                    }
                } else {
                    buckle_core::testing::ok_output(&show(
                        "loaded",
                        "active",
                        "running",
                        "Mon 2026-01-05 09:00:00 UTC",
                    ))
                }
            });
        let result = SystemdCheck
            .check(
                &cx(&sys),
                &spec(json!({"unit": "nomad", "min_uptime": "30s"})),
            )
            .await;
        assert_eq!(result, HealthStatus::Pass);
        assert_eq!(sys.commands_run().len(), 2);
    }

    #[test]
    fn timestamps_parse() {
        assert_eq!(
            parse_timestamp("Mon 2026-01-05 10:00:00 UTC").unwrap(),
            Some(
                DateTime::parse_from_rfc3339("2026-01-05T10:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert!(
            parse_timestamp("2026-01-05 10:00:00.123456 UTC")
                .unwrap()
                .is_some()
        );
        assert_eq!(parse_timestamp("").unwrap(), None);
        assert_eq!(parse_timestamp("n/a").unwrap(), None);
    }
}
