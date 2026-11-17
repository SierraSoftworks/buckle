//! `schedules/<name>.yaml`: an action rolled out on a cron schedule.
//!
//! ```yaml
//! # schedules/weekly-upgrades.yaml
//! description: Upgrade the home hosts' packages every Sunday morning.
//! cron: "0 4 * * SUN"
//! timezone: Europe/London
//! module: packages
//! action: upgrade-reboot
//! select: 'host.groups contains "home"'
//! ```

use crate::Filter;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::Duration;

/// One schedule: each occurrence of `cron` rolls `action` out (as an
/// operator's Run action… would) over the targets offering it that
/// `select` matches, through the stable revision's phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDecl {
    /// What it is for, shown with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A cron expression: five fields (minute granularity), six with
    /// seconds, or an alias such as `@daily`.
    pub cron: String,
    /// The IANA time zone `cron` is read in.
    #[serde(default = "ScheduleDecl::default_timezone")]
    pub timezone: String,
    /// The host module declaring the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The workload declaring the action, run on each of its placements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    pub action: String,
    /// A filter over the targets offering the action; every one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Filter>,
    /// How long an occurrence may wait for a rollout under way to finish
    /// (or a server that was down to come back) before it is skipped.
    #[serde(default = "ScheduleDecl::default_window", with = "humantime_serde")]
    pub window: Duration,
    /// Whether the phases' approvals apply to its rollouts.
    #[serde(default)]
    pub approval: ScheduleApproval,
    /// Kept in the audit log with each rollout; required for an action that
    /// asks for confirmation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Whether a schedule's rollouts wait for the approvals its phases ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleApproval {
    /// As the stable revision's phases say: one requiring approval waits
    /// for an operator.
    #[default]
    Phases,
    /// Every phase goes ahead unattended; the commit declaring the schedule
    /// is the sign-off.
    Auto,
}

impl ScheduleDecl {
    pub fn parse(yaml: &str) -> crate::Result<Self> {
        use crate::error::HumanizableResult;
        serde_yaml::from_str(yaml).to_human_error()
    }

    fn default_timezone() -> String {
        "UTC".into()
    }

    fn default_window() -> Duration {
        Duration::from_secs(3600)
    }

    /// The module (or workload) declaring the action, and whether it is a
    /// workload.
    pub fn target(&self) -> Option<(&str, bool)> {
        match (&self.module, &self.workload) {
            (Some(m), None) => Some((m, false)),
            (None, Some(w)) => Some((w, true)),
            _ => None,
        }
    }

    /// `cron` and `timezone`, parsed.
    pub fn parsed(&self) -> Result<Parsed, String> {
        let cron = croner::Cron::from_str(&self.cron)
            .map_err(|e| format!("`cron: {}` is not a cron expression: {e}", self.cron))?;
        let tz = chrono_tz::Tz::from_str(&self.timezone).map_err(|_| {
            format!(
                "`timezone: {}` is not an IANA time zone (such as Europe/London or UTC)",
                self.timezone
            )
        })?;
        Ok(Parsed { cron, tz })
    }

    /// Problems with the declaration itself (not with what it names).
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.target().is_none() {
            out.push("names exactly one of `module` (a host module's action) or `workload` (a workload's)".into());
        }
        if let Err(e) = self.parsed() {
            out.push(e);
        }
        if self.window.is_zero() {
            out.push("`window` must be longer than zero".into());
        }
        out
    }
}

/// A schedule's expression in its time zone.
#[derive(Debug, Clone)]
pub struct Parsed {
    cron: croner::Cron,
    tz: chrono_tz::Tz,
}

impl Parsed {
    /// The first occurrence strictly after `at`.
    pub fn next_after(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.cron
            .find_next_occurrence(&at.with_timezone(&self.tz), false)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// The latest occurrence at or before `at`.
    pub fn latest_until(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.cron
            .find_previous_occurrence(&at.with_timezone(&self.tz), true)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// The expression in words (`At 04:00 on Sunday`).
    pub fn describe(&self) -> String {
        self.cron.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(yaml: &str) -> ScheduleDecl {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn occurrences_follow_the_time_zone() {
        let d = decl(
            "cron: '0 4 * * SUN'\ntimezone: Europe/London\nmodule: packages\naction: upgrade\n",
        );
        let p = d.parsed().unwrap();
        // 2026-07-05 is a Sunday in BST (UTC+1); 2026-12-06 one in GMT.
        let cases = [
            ("2026-07-01T00:00:00Z", "2026-07-05T03:00:00Z"),
            ("2026-12-01T00:00:00Z", "2026-12-06T04:00:00Z"),
            ("2026-12-06T04:00:00Z", "2026-12-13T04:00:00Z"),
        ];
        for (from, want) in cases {
            assert_eq!(p.next_after(at(from)), Some(at(want)), "after {from}");
        }
        assert_eq!(
            p.latest_until(at("2026-12-06T04:00:00Z")),
            Some(at("2026-12-06T04:00:00Z"))
        );
    }

    #[test]
    fn defaults_and_problems() {
        let d = decl("cron: '@daily'\nworkload: grafana\naction: restart\n");
        assert_eq!(d.timezone, "UTC");
        assert_eq!(d.window, Duration::from_secs(3600));
        assert_eq!(d.approval, ScheduleApproval::Phases);
        assert_eq!(d.target(), Some(("grafana", true)));
        assert!(d.problems().is_empty(), "{:?}", d.problems());

        for bad in [
            "cron: 'nope'\nmodule: m\naction: a\n",
            "cron: '@daily'\ntimezone: Mars/Olympus\nmodule: m\naction: a\n",
            "cron: '@daily'\naction: a\n",
            "cron: '@daily'\nmodule: m\nworkload: w\naction: a\n",
        ] {
            assert_eq!(decl(bad).problems().len(), 1, "{bad}");
        }
        assert!(
            serde_yaml::from_str::<ScheduleDecl>("cron: x\nmodule: m\naction: a\nbogus: 1\n")
                .is_err()
        );
    }
}
