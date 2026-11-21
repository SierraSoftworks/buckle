//! A rollout's module and workload limits ([`RolloutLimit`]) as lines to
//! read at a glance: how much of each `parallel` is in use, what queues
//! behind it, and whether a failure stopped its other targets.

use buckle_api::{RolloutDetail, RolloutLimit, RolloutLimitKind};

use super::format::count;
use super::tone::Tone;

/// One module's or workload's limits, as the rollout page shows them.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitLine {
    /// `module` or `workload`.
    pub layer: &'static str,
    pub name: String,
    /// The places of its `parallel`; `None` when it sets none.
    pub places: Option<usize>,
    pub in_flight: usize,
    /// Places failed targets keep.
    pub held: usize,
    /// In use and queued, in words.
    pub usage: String,
    /// What failed on it against its `max_failures`, and how grave that is:
    /// [`Tone::Danger`] once its other targets stopped.
    pub failures: Option<(Tone, String)>,
    /// The limits as declared, for a hover.
    pub tip: String,
}

impl LimitLine {
    /// Whether nothing more of it starts until an operator acts: a failure
    /// beyond its `max_failures`, or every place held by a failed target.
    pub fn stopped(&self) -> bool {
        matches!(self.failures, Some((Tone::Danger, _)))
            || self
                .places
                .is_some_and(|p| self.in_flight == 0 && self.held >= p)
    }
}

/// The limits of `detail` that say something now: one in use, with targets
/// queued behind it, or with a failure on it. A rollout whose limited
/// targets all finished shows none.
pub fn lines(detail: &RolloutDetail) -> Vec<LimitLine> {
    detail
        .limits
        .iter()
        .filter(|l| l.in_flight > 0 || l.queued > 0 || !l.failures.is_empty())
        .map(line)
        .collect()
}

fn line(l: &RolloutLimit) -> LimitLine {
    let (in_flight, held, queued) = (l.in_flight as usize, l.holding.len(), l.queued as usize);
    let places = l.parallel.map(|p| p as usize);
    let mut usage = vec![match places {
        Some(p) => format!("{in_flight} of {p} in flight"),
        None => format!("{in_flight} in flight"),
    }];
    if places.is_some() && held > 0 {
        usage.push(format!("{held} held by a failure"));
    }
    if queued > 0 {
        usage.push(format!("{queued} queued"));
    }
    let failures = (!l.failures.is_empty()).then(|| {
        let failed = l.failures.len();
        let on = l.failures.join(", ");
        if failed > l.max_failures as usize {
            (
                Tone::Danger,
                format!(
                    "failed on {on} (max_failures {}): its other targets wait for a retry or skip there",
                    l.max_failures
                ),
            )
        } else {
            (
                Tone::Warn,
                format!(
                    "{} of {} tolerated: {on}",
                    count(failed, "failure", "failures"),
                    l.max_failures
                ),
            )
        }
    });
    let layer = match l.kind {
        RolloutLimitKind::Module => "module",
        RolloutLimitKind::Workload => "workload",
    };
    let tip = format!(
        "{} of this rollout's targets run {layer} {}: {}, max_failures {}",
        l.total,
        l.name,
        match places {
            Some(p) => format!("{p} at a time (parallel)"),
            None => "no parallel of its own".to_owned(),
        },
        l.max_failures
    );
    LimitLine {
        layer,
        tip,
        name: l.name.clone(),
        places,
        in_flight,
        held,
        usage: usage.join(" · "),
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(name: &str) -> RolloutLimit {
        RolloutLimit {
            kind: RolloutLimitKind::Module,
            name: name.to_owned(),
            parallel: Some(1),
            max_failures: 0,
            total: 3,
            in_flight: 0,
            queued: 0,
            holding: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn only(l: RolloutLimit) -> LimitLine {
        line(&l)
    }

    #[test]
    fn a_limit_in_use_reads_its_places_and_queue() {
        let l = only(RolloutLimit {
            in_flight: 1,
            queued: 2,
            ..limit("nomad")
        });
        assert_eq!((l.layer, l.places, l.in_flight), ("module", Some(1), 1));
        assert!(l.usage.contains("1 of 1") && l.usage.contains("2 queued"));
        assert!(l.failures.is_none() && !l.stopped());
    }

    #[test]
    fn a_failure_beyond_the_budget_stops_the_rest() {
        let l = only(RolloutLimit {
            queued: 2,
            holding: vec!["lab-1".into()],
            failures: vec!["lab-1".into()],
            ..limit("nomad")
        });
        let (tone, text) = l.failures.clone().expect("the failure is shown");
        assert_eq!(tone, Tone::Danger);
        assert!(text.contains("lab-1") && text.contains("max_failures 0"));
        assert!(l.stopped());
        assert!(l.usage.contains("held"));
    }

    #[test]
    fn a_tolerated_failure_warns_without_stopping() {
        let l = only(RolloutLimit {
            parallel: Some(2),
            max_failures: 1,
            in_flight: 1,
            holding: vec!["lab-1".into()],
            failures: vec!["lab-1".into()],
            ..limit("nomad")
        });
        assert_eq!(l.failures.as_ref().map(|f| f.0), Some(Tone::Warn));
        assert!(!l.stopped());
    }

    #[test]
    fn a_failed_target_holding_every_place_stops_it_whoever_is_charged() {
        let l = only(RolloutLimit {
            queued: 1,
            holding: vec!["lab-1".into()],
            ..limit("nomad")
        });
        assert!(l.failures.is_none() && l.stopped());
    }

    #[test]
    fn only_limits_with_something_to_say_are_listed() {
        let mut d = crate::fixtures::data::active_rollout(chrono::Utc::now());
        d.limits = vec![
            limit("idle"),
            RolloutLimit {
                in_flight: 1,
                ..limit("busy")
            },
            RolloutLimit {
                queued: 1,
                ..limit("queued")
            },
            RolloutLimit {
                parallel: None,
                failures: vec!["lab-2".into()],
                ..limit("failed")
            },
        ];
        let names: Vec<String> = lines(&d).into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["busy", "queued", "failed"]);
    }
}
