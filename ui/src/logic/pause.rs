//! Suspending one module of a host, or one workload of a cluster: what its
//! row offers, how a suspended unit reads, and the review date its dialog
//! asks for.

use buckle_api::{ActionInfo, LiveEvent, PausedUnit};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

use super::format::relative;

/// A unit the operator asked to suspend or resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseAsk {
    /// The host, or the cluster target.
    pub target: String,
    /// The module, or the workload.
    pub unit: String,
    pub workload: bool,
    /// How it is suspended now: the ask is to resume it.
    pub paused: Option<PausedUnit>,
}

/// What a unit's row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Suspend,
    Resume,
}

impl Offer {
    pub fn label(self) -> &'static str {
        match self {
            Offer::Suspend => "Suspend…",
            Offer::Resume => "Resume…",
        }
    }

    pub fn title(self, workload: bool) -> &'static str {
        match (self, workload) {
            (Offer::Suspend, false) => {
                "Leave this module alone on this host: rollouts, drift correction and actions pass it by"
            }
            (Offer::Suspend, true) => {
                "Stop this workload on this cluster and leave it alone: rollouts, drift correction and actions pass it by"
            }
            (Offer::Resume, _) => "Lift the pause and bring it to the applied revision",
        }
    }
}

/// A suspended unit can always be resumed; suspending needs the target
/// active and its agent (a cluster's steward) connected, since the agent
/// keeps the pause.
pub fn offer(paused: bool, reachable: bool) -> Option<Offer> {
    match (paused, reachable) {
        (true, _) => Some(Offer::Resume),
        (false, true) => Some(Offer::Suspend),
        (false, false) => None,
    }
}

/// A target's actions without those of its suspended units: nothing of a
/// suspended unit runs until it is resumed.
pub fn runnable(actions: &[ActionInfo], paused: &[PausedUnit]) -> Vec<ActionInfo> {
    actions
        .iter()
        .filter(|a| !paused.iter().any(|p| p.unit == a.module))
        .cloned()
        .collect()
}

/// The review date a dialog's date field holds (`2026-12-01`, or blank for
/// none) as the start of that day, UTC.
pub fn review_at(date: &str) -> Result<Option<DateTime<Utc>>, String> {
    let date = date.trim();
    if date.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| Some(d.and_time(NaiveTime::MIN).and_utc()))
        .map_err(|_| format!("'{date}' is not a date (YYYY-MM-DD)."))
}

/// Whether the day the operator meant to look at it again has come.
pub fn review_due(p: &PausedUnit, now: DateTime<Utc>) -> bool {
    p.review_at.is_some_and(|at| at <= now)
}

/// `review due since 2026-12-01`, or `review on 2026-12-01`.
pub fn review_text(p: &PausedUnit, now: DateTime<Utc>) -> Option<String> {
    let at = p.review_at?;
    let day = at.format("%Y-%m-%d");
    Some(if at <= now {
        format!("review due since {day}")
    } else {
        format!("review on {day}")
    })
}

/// Who suspended it and when: `by ada@example.com 3h ago`.
pub fn attribution(p: &PausedUnit, now: DateTime<Utc>) -> String {
    format!("by {} {}", p.by, relative(p.at, now))
}

/// Whether `event` says a unit was suspended or resumed somewhere: a list
/// of them is fetched again.
pub fn changes_units(event: &LiveEvent) -> bool {
    matches!(event, LiveEvent::Audit { entry } if entry.action.starts_with("target.unit."))
}

/// Fleet-wide, the units wanting a look first (review due, oldest date
/// first), then the rest, longest suspended first.
pub fn listed(mut units: Vec<PausedUnit>, now: DateTime<Utc>) -> Vec<PausedUnit> {
    units.sort_by(|a, b| {
        let key = |p: &PausedUnit| {
            (
                !review_due(p, now),
                p.review_at.filter(|at| *at <= now),
                p.at,
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| (&a.target, &a.unit).cmp(&(&b.target, &b.unit)))
    });
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap()
    }

    fn unit(name: &str, days_ago: i64, review_in_days: Option<i64>) -> PausedUnit {
        PausedUnit {
            target: "edge-1".into(),
            unit: name.into(),
            note: "CVE in the image".into(),
            by: "ada@example.com".into(),
            at: now() - Duration::days(days_ago),
            review_at: review_in_days.map(|d| now() + Duration::days(d)),
        }
    }

    #[test]
    fn a_row_offers_what_its_unit_can_take() {
        for (paused, reachable, want) in [
            (false, true, Some(Offer::Suspend)),
            (false, false, None),
            (true, true, Some(Offer::Resume)),
            (true, false, Some(Offer::Resume)),
        ] {
            assert_eq!(
                offer(paused, reachable),
                want,
                "paused={paused} reachable={reachable}"
            );
        }
    }

    #[test]
    fn a_suspended_units_actions_are_not_offered() {
        let action = |module: &str| ActionInfo {
            module: module.into(),
            name: "restart".into(),
            label: "Restart".into(),
            kind: "command.run".into(),
            description: None,
            confirm: false,
            implicit: false,
            instances: None,
        };
        let left = runnable(&[action("web"), action("db")], &[unit("web", 1, None)]);
        let modules: Vec<&str> = left.iter().map(|a| a.module.as_str()).collect();
        assert_eq!(modules, ["db"]);
    }

    #[test]
    fn a_review_date_is_optional_and_starts_its_day() {
        assert_eq!(review_at("  "), Ok(None));
        assert_eq!(
            review_at("2026-12-01"),
            Ok(Some(Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap()))
        );
        assert!(review_at("next week").is_err());
    }

    #[test]
    fn a_review_reads_as_due_once_its_day_has_come() {
        for (review, due, text) in [
            (None, false, None),
            (Some(3), false, Some("review on 2026-11-25")),
            (Some(-2), true, Some("review due since 2026-11-20")),
        ] {
            let p = unit("web", 5, review);
            assert_eq!(review_due(&p, now()), due, "{review:?}");
            assert_eq!(review_text(&p, now()).as_deref(), text, "{review:?}");
        }
    }

    #[test]
    fn units_due_for_review_are_listed_first() {
        let order: Vec<String> = listed(
            vec![
                unit("recent", 1, None),
                unit("old", 9, Some(4)),
                unit("due", 3, Some(-1)),
                unit("overdue", 2, Some(-6)),
            ],
            now(),
        )
        .into_iter()
        .map(|p| p.unit)
        .collect();
        assert_eq!(order, ["overdue", "due", "old", "recent"]);
    }

    #[test]
    fn only_suspending_and_resuming_change_the_list() {
        let audit = |action: &str| LiveEvent::Audit {
            entry: buckle_api::AuditEntryRef {
                id: 1,
                at: now(),
                action: action.into(),
                message: String::new(),
            },
        };
        for (action, want) in [
            ("target.unit.suspend", true),
            ("target.unit.resume", true),
            ("host.action.run", false),
        ] {
            assert_eq!(changes_units(&audit(action)), want, "{action}");
        }
    }

    #[test]
    fn a_suspended_unit_says_who_and_when() {
        assert!(attribution(&unit("web", 2, None), now()).starts_with("by ada@example.com"));
    }
}
