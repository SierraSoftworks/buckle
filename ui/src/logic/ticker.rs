//! A host's way through a rollout as a ticker: the states it was in over
//! time, as segments of a bar, and every event of its trail as a marker over
//! it. The server's audit trail keeps outcomes (healthy, failed, held, …)
//! but not ordinary progress, so applying and baking are also read from the
//! host's run: applying from its start, baking from its end. A reboot is in
//! the trail: rebooting from the run's result, baking once the host is back.

use buckle_api::{HostRolloutState, RolloutEvent};
use chrono::{DateTime, Duration, Utc};

/// One state, held from `from` until `until`.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub state: HostRolloutState,
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// Still the host's state: it runs to now.
    pub ongoing: bool,
}

/// An event of the host's trail, placed over the bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker<'a> {
    pub event: &'a RolloutEvent,
    /// The state the event moved the host to, when it is a transition.
    pub transition: Option<HostRolloutState>,
    /// Which row of markers it sits in, so close events do not overlap.
    pub lane: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ticker<'a> {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub segments: Vec<Segment>,
    pub markers: Vec<Marker<'a>>,
    /// The end of a timed state still under way (a bake, or the wait for a
    /// reboot): the bar runs on to it, dashed.
    pub planned: Option<(HostRolloutState, DateTime<Utc>)>,
}

impl Ticker<'_> {
    /// Where `at` falls along the bar, from 0 to 100.
    pub fn pct(&self, at: DateTime<Utc>) -> f64 {
        let span = (self.until - self.from).num_milliseconds().max(1) as f64;
        ((at - self.from).num_milliseconds() as f64 / span * 100.0).clamp(0.0, 100.0)
    }

    pub fn lanes(&self) -> usize {
        self.markers.iter().map(|m| m.lane + 1).max().unwrap_or(0)
    }
}

/// What the ticker is drawn from.
pub struct Source<'a> {
    pub events: Vec<&'a RolloutEvent>,
    pub state: HostRolloutState,
    pub run_started: Option<DateTime<Utc>>,
    pub run_finished: Option<DateTime<Utc>>,
    pub bake_until: Option<DateTime<Utc>>,
    /// When a rebooting host must be back by.
    pub reboot_until: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

/// Markers closer than this (in percent of the bar) go in separate lanes.
const MARKER_GAP: f64 = 2.5;
const MAX_LANES: usize = 3;

/// The state a `rollout.host.<state>` transition moved a host to, as the
/// API names host states.
pub fn transition(kind: &str) -> Option<HostRolloutState> {
    Some(match kind.strip_prefix("rollout.host.")? {
        "pending" | "missed" => HostRolloutState::Pending,
        "plan_pending" => HostRolloutState::PlanPending,
        "planned" | "baseline" => HostRolloutState::Planned,
        "held" => HostRolloutState::Held,
        "applying" | "rolling_back" => HostRolloutState::Applying,
        "rebooting" => HostRolloutState::Rebooting,
        "baking" | "rebooted" => HostRolloutState::Baking,
        "healthy" => HostRolloutState::Healthy,
        "failed" => HostRolloutState::Failed,
        "skipped" => HostRolloutState::Skipped,
        "rolled_back" => HostRolloutState::RolledBack,
        _ => return None,
    })
}

/// A state the host does not leave by itself: its segment ends where it
/// begins, as the bar's last mark.
fn settled(state: HostRolloutState) -> bool {
    matches!(
        state,
        HostRolloutState::Healthy
            | HostRolloutState::Failed
            | HostRolloutState::Skipped
            | HostRolloutState::RolledBack
    )
}

pub fn ticker(src: Source<'_>) -> Option<Ticker<'_>> {
    let mut events = src.events;
    events.sort_by_key(|e| e.at);
    let mut points: Vec<(DateTime<Utc>, HostRolloutState)> = events
        .iter()
        .filter_map(|e| transition(&e.kind).map(|s| (e.at, s)))
        .collect();
    let has = |points: &[(DateTime<Utc>, HostRolloutState)], s| points.iter().any(|p| p.1 == s);
    if let Some(at) = src
        .run_started
        .filter(|_| !has(&points, HostRolloutState::Applying))
    {
        points.push((at, HostRolloutState::Applying));
    }
    let baked = src.bake_until.is_some()
        || matches!(
            src.state,
            HostRolloutState::Baking | HostRolloutState::Healthy
        )
        || has(&points, HostRolloutState::Healthy);
    if let Some(at) = src
        .run_finished
        .filter(|_| baked && !has(&points, HostRolloutState::Baking))
    {
        points.push((at, HostRolloutState::Baking));
    }
    points.sort_by_key(|p| p.0);

    let segments: Vec<Segment> = points
        .iter()
        .enumerate()
        .filter_map(|(i, &(from, state))| match points.get(i + 1) {
            Some(&(until, _)) => Some(Segment {
                state,
                from,
                until,
                ongoing: false,
            }),
            None if settled(state) => None,
            None => Some(Segment {
                state,
                from,
                until: src.now.max(from),
                ongoing: true,
            }),
        })
        .collect();

    let times = events
        .iter()
        .map(|e| e.at)
        .chain(points.iter().map(|p| p.0))
        .chain(segments.iter().map(|s| s.until));
    let (mut from, mut until) = times.fold((None, None), |(lo, hi), t| {
        (
            Some(lo.map_or(t, |l: DateTime<Utc>| l.min(t))),
            Some(hi.map_or(t, |h: DateTime<Utc>| h.max(t))),
        )
    });
    let planned = match src.state {
        HostRolloutState::Baking => src.bake_until,
        HostRolloutState::Rebooting => src.reboot_until,
        _ => None,
    }
    .filter(|b| *b > src.now)
    .map(|b| (src.state, b));
    if let Some((_, b)) = planned {
        until = until.map(|u| u.max(b));
    }
    let (from, until) = (from.take()?, until.take()?);
    let until = until.max(from + Duration::seconds(1));

    let mut t = Ticker {
        from,
        until,
        segments,
        markers: Vec::new(),
        planned,
    };
    // Each marker takes the first lane its neighbour on the left leaves room in.
    let mut ends: Vec<f64> = Vec::new();
    t.markers = events
        .into_iter()
        .map(|event| {
            let at = t.pct(event.at);
            let lane = ends
                .iter()
                .position(|end| at - end >= MARKER_GAP)
                .unwrap_or_else(|| ends.len().min(MAX_LANES - 1));
            if lane == ends.len() {
                ends.push(at);
            } else {
                ends[lane] = at;
            }
            Marker {
                event,
                transition: transition(&event.kind),
                lane,
            }
        })
        .collect();
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(secs: i64) -> DateTime<Utc> {
        t0() + Duration::seconds(secs)
    }

    fn ev(secs: i64, kind: &str) -> RolloutEvent {
        RolloutEvent {
            at: at(secs),
            kind: kind.into(),
            message: kind.into(),
            host: Some("pi-1".into()),
            workload: None,
            phase: Some("canary".into()),
            actor: Some("controller".into()),
        }
    }

    fn source(events: &[RolloutEvent], state: HostRolloutState) -> Source<'_> {
        Source {
            events: events.iter().collect(),
            state,
            run_started: None,
            run_finished: None,
            bake_until: None,
            reboot_until: None,
            now: at(600),
        }
    }

    fn states(t: &Ticker<'_>) -> Vec<(HostRolloutState, i64, i64)> {
        t.segments
            .iter()
            .map(|s| {
                (
                    s.state,
                    (s.from - t0()).num_seconds(),
                    (s.until - t0()).num_seconds(),
                )
            })
            .collect()
    }

    #[test]
    fn only_state_changes_are_transitions() {
        for (kind, want) in [
            ("rollout.host.applying", Some(HostRolloutState::Applying)),
            (
                "rollout.host.rolled_back",
                Some(HostRolloutState::RolledBack),
            ),
            ("rollout.host.baseline", Some(HostRolloutState::Planned)),
            ("rollout.host.skip", None),
            ("rollout.host.retry", None),
            ("rollout.paused", None),
        ] {
            assert_eq!(transition(kind), want, "{kind}");
        }
    }

    #[test]
    fn a_host_with_no_trail_and_no_run_has_no_ticker() {
        assert!(ticker(source(&[], HostRolloutState::Pending)).is_none());
    }

    #[test]
    fn transitions_become_segments_and_a_settled_state_ends_the_bar() {
        let events = [
            ev(10, "rollout.host.applying"),
            ev(70, "rollout.host.baking"),
            ev(370, "rollout.host.healthy"),
        ];
        let t = ticker(source(&events, HostRolloutState::Healthy)).unwrap();
        use HostRolloutState::*;
        assert_eq!(states(&t), vec![(Applying, 10, 70), (Baking, 70, 370)]);
        assert_eq!((t.from, t.until), (at(10), at(370)));
        assert_eq!(t.markers.len(), 3, "every event is a marker");
        assert_eq!(t.markers[2].transition, Some(Healthy));
    }

    #[test]
    fn the_run_fills_in_progress_the_trail_does_not_keep() {
        let events = [ev(400, "rollout.host.healthy")];
        let src = Source {
            run_started: Some(at(20)),
            run_finished: Some(at(80)),
            ..source(&events, HostRolloutState::Healthy)
        };
        let t = ticker(src).unwrap();
        use HostRolloutState::*;
        assert_eq!(states(&t), vec![(Applying, 20, 80), (Baking, 80, 400)]);
    }

    #[test]
    fn a_failed_run_does_not_bake() {
        let events = [ev(90, "rollout.host.failed")];
        let src = Source {
            run_started: Some(at(20)),
            run_finished: Some(at(80)),
            ..source(&events, HostRolloutState::Failed)
        };
        let t = ticker(src).unwrap();
        use HostRolloutState::*;
        assert_eq!(states(&t), vec![(Applying, 20, 90)]);
    }

    #[test]
    fn a_bake_under_way_runs_to_now_and_on_to_its_end() {
        let src = Source {
            run_started: Some(at(20)),
            run_finished: Some(at(80)),
            bake_until: Some(at(900)),
            ..source(&[], HostRolloutState::Baking)
        };
        let t = ticker(src).unwrap();
        let last = t.segments.last().unwrap();
        assert!(last.ongoing);
        assert_eq!(last.until, at(600), "to now");
        assert_eq!(t.until, at(900), "the bar shows what is left");
        assert_eq!(t.planned, Some((HostRolloutState::Baking, at(900))));
    }

    #[test]
    fn a_reboot_sits_between_the_run_and_its_bake() {
        use HostRolloutState::*;
        let rebooting = [ev(81, "rollout.host.rebooting")];
        let src = Source {
            run_started: Some(at(20)),
            run_finished: Some(at(80)),
            reboot_until: Some(at(900)),
            ..source(&rebooting, Rebooting)
        };
        let t = ticker(src).unwrap();
        assert_eq!(states(&t), vec![(Applying, 20, 81), (Rebooting, 81, 600)]);
        assert_eq!(t.planned, Some((Rebooting, at(900))), "the wait runs on");

        let back = [rebooting[0].clone(), ev(200, "rollout.host.rebooted")];
        let src = Source {
            run_started: Some(at(20)),
            run_finished: Some(at(80)),
            bake_until: Some(at(900)),
            ..source(&back, Baking)
        };
        let t = ticker(src).unwrap();
        assert_eq!(
            states(&t),
            vec![(Applying, 20, 81), (Rebooting, 81, 200), (Baking, 200, 600)]
        );
    }

    #[test]
    fn close_markers_take_separate_lanes() {
        let events = [
            ev(0, "host.baseline"),
            ev(1, "rollout.host.applying"),
            ev(2, "host.applied"),
            ev(500, "rollout.host.healthy"),
        ];
        let t = ticker(source(&events, HostRolloutState::Healthy)).unwrap();
        let lanes: Vec<usize> = t.markers.iter().map(|m| m.lane).collect();
        assert_eq!(lanes, vec![0, 1, 2, 0]);
        assert_eq!(t.lanes(), 3);
    }
}
