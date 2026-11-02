//! Health readings as states over time: how bad a reading is, and a module's
//! readings folded into equal time buckets, each taking the worst state of
//! its checks, for the run report's health bar.

use buckle_api::HealthResultDto;
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

use super::tone::Tone;

/// A reading's state, least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckState {
    Pass,
    /// An informational check failing: worth seeing, never gating.
    Informational,
    /// The check could not run.
    Unknown,
    Fail,
}

impl CheckState {
    pub fn of(status: &str, required: bool) -> Self {
        match status {
            "pass" => Self::Pass,
            "fail" if !required => Self::Informational,
            "fail" => Self::Fail,
            _ => Self::Unknown,
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::Pass => Tone::Ok,
            Self::Informational => Tone::Mute,
            Self::Unknown => Tone::Warn,
            Self::Fail => Tone::Danger,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "passing",
            Self::Informational => "failing (informational)",
            Self::Unknown => "unknown",
            Self::Fail => "failing",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Informational => "info",
            Self::Unknown => "unknown",
            Self::Fail => "fail",
        }
    }
}

/// One check within a bucket: its worst reading in the bucket, or the last
/// one before it when it did not run in the bucket (`carried`).
#[derive(Debug, Clone, PartialEq)]
pub struct BucketCheck<'a> {
    pub reading: &'a HealthResultDto,
    pub state: CheckState,
    pub carried: bool,
}

/// One slice of the bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket<'a> {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// The worst state of its checks; `None` before any check reported.
    pub state: Option<CheckState>,
    /// Ordered by check name.
    pub checks: Vec<BucketCheck<'a>>,
}

/// The worst state among the latest reading of each check.
pub fn current_state(readings: &[HealthResultDto]) -> Option<CheckState> {
    let mut latest: BTreeMap<&str, &HealthResultDto> = BTreeMap::new();
    for r in readings {
        let slot = latest.entry(r.check.as_str()).or_insert(r);
        if r.at >= slot.at {
            *slot = r;
        }
    }
    latest
        .values()
        .map(|r| CheckState::of(&r.status, r.required))
        .max()
}

/// Folds `readings` into `count` equal buckets spanning from `from` (or the
/// first reading, when earlier) to the last reading.
pub fn timeline(
    readings: &[HealthResultDto],
    from: Option<DateTime<Utc>>,
    count: usize,
) -> Vec<Bucket<'_>> {
    let mut sorted: Vec<&HealthResultDto> = readings.iter().collect();
    sorted.sort_by_key(|r| r.at);
    let (Some(first), Some(last)) = (sorted.first(), sorted.last()) else {
        return Vec::new();
    };
    let count = count.max(1);
    let start = from.map_or(first.at, |f| f.min(first.at));
    // The last reading falls inside the last bucket.
    let span = (last.at - start).max(Duration::seconds(1)) + Duration::milliseconds(1);
    let width = span / count as i32;
    let mut known: BTreeMap<&str, BucketCheck<'_>> = BTreeMap::new();
    let mut next = sorted.into_iter().peekable();
    (0..count)
        .map(|i| {
            let from = start + width * i as i32;
            let until = if i + 1 == count {
                start + span
            } else {
                from + width
            };
            for c in known.values_mut() {
                c.carried = true;
            }
            while let Some(r) = next.next_if(|r| r.at < until) {
                let state = CheckState::of(&r.status, r.required);
                let fresh = BucketCheck {
                    reading: r,
                    state,
                    carried: false,
                };
                known
                    .entry(r.check.as_str())
                    .and_modify(|c| {
                        if c.carried || state >= c.state {
                            *c = fresh.clone();
                        }
                    })
                    .or_insert(fresh);
            }
            let checks: Vec<BucketCheck<'_>> = known.values().cloned().collect();
            Bucket {
                from,
                until,
                state: checks.iter().map(|c| c.state).max(),
                checks,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(min: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(min)
    }

    fn reading(check: &str, status: &str, min: i64) -> HealthResultDto {
        HealthResultDto {
            scope: "bake".into(),
            module: "web".into(),
            check: check.into(),
            kind: "http".into(),
            required: check != "metrics",
            status: status.into(),
            reason: None,
            at: at(min),
            rollout_id: None,
        }
    }

    fn states(buckets: &[Bucket<'_>]) -> Vec<Option<CheckState>> {
        buckets.iter().map(|b| b.state).collect()
    }

    #[test]
    fn states_from_status() {
        for (status, required, want) in [
            ("pass", true, CheckState::Pass),
            ("fail", true, CheckState::Fail),
            ("fail", false, CheckState::Informational),
            ("unknown", true, CheckState::Unknown),
        ] {
            assert_eq!(
                CheckState::of(status, required),
                want,
                "{status} required={required}"
            );
        }
    }

    #[test]
    fn severity_puts_required_failures_first() {
        assert!(CheckState::Fail > CheckState::Unknown);
        assert!(CheckState::Unknown > CheckState::Informational);
        assert!(CheckState::Informational > CheckState::Pass);
    }

    #[test]
    fn no_readings_no_buckets() {
        assert!(timeline(&[], Some(at(0)), 10).is_empty());
    }

    #[test]
    fn a_bucket_takes_the_worst_reading_within_it() {
        let readings = vec![
            reading("api", "pass", 0),
            reading("api", "fail", 1),
            reading("api", "pass", 2),
            reading("api", "pass", 9),
        ];
        let buckets = timeline(&readings, None, 3);
        assert_eq!(buckets.len(), 3);
        assert_eq!(
            buckets[0].state,
            Some(CheckState::Fail),
            "a blip within a bucket shows"
        );
        assert_eq!(buckets[2].state, Some(CheckState::Pass));
        assert_eq!(buckets[0].checks[0].reading.status, "fail");
    }

    #[test]
    fn quiet_buckets_carry_the_last_reading_forward() {
        let readings = vec![reading("api", "fail", 0), reading("api", "pass", 9)];
        let buckets = timeline(&readings, None, 3);
        assert_eq!(
            states(&buckets),
            vec![
                Some(CheckState::Fail),
                Some(CheckState::Fail),
                Some(CheckState::Pass)
            ]
        );
        assert!(buckets[1].checks[0].carried);
        assert!(!buckets[2].checks[0].carried);
    }

    #[test]
    fn buckets_before_the_first_reading_have_no_state() {
        let readings = vec![reading("api", "pass", 9)];
        let buckets = timeline(&readings, Some(at(0)), 3);
        assert_eq!(states(&buckets), vec![None, None, Some(CheckState::Pass)]);
        assert!(buckets[0].checks.is_empty());
    }

    #[test]
    fn a_bucket_is_as_bad_as_its_worst_check_and_lists_each_check() {
        let readings = vec![
            reading("api", "pass", 0),
            reading("metrics", "fail", 0),
            reading("db", "unknown", 1),
        ];
        let buckets = timeline(&readings, None, 1);
        assert_eq!(buckets[0].state, Some(CheckState::Unknown));
        let names: Vec<&str> = buckets[0]
            .checks
            .iter()
            .map(|c| c.reading.check.as_str())
            .collect();
        assert_eq!(names, ["api", "db", "metrics"]);
    }

    #[test]
    fn current_state_reads_each_checks_latest_reading() {
        let readings = vec![
            reading("api", "fail", 0),
            reading("api", "pass", 5),
            reading("metrics", "fail", 1),
        ];
        assert_eq!(current_state(&readings), Some(CheckState::Informational));
        assert_eq!(current_state(&[]), None);
    }
}
