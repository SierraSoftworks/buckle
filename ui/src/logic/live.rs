//! How a Server-Sent Event changes what a page shows.
//!
//! A page fetches its data once and then lets the event stream keep it
//! current: every event after the fetch is applied to the fetched value by one
//! of the `apply_*` functions here. Events replace whole objects (a host, a
//! rollout summary, one host's rollout entry), so applying one twice, or
//! applying one the fetch already reflected, changes nothing.

use std::collections::VecDeque;
use std::rc::Rc;

use buckle_api::{
    Connection, Fleet, FleetCounts, HealthSummary, HostDetail, HostStatus, HostSummary, LiveEvent,
    RevisionSummary, RolloutDetail, RolloutSummary, TargetKind,
};

use super::tone::rollout_active;

/// Recomputes the fleet's counts from its hosts.
pub fn recount(hosts: &[HostSummary]) -> FleetCounts {
    let mut c = FleetCounts {
        total: hosts.len() as u32,
        ..FleetCounts::default()
    };
    for h in hosts {
        c.online += u32::from(h.connection == Connection::Online);
        c.healthy += u32::from(h.health == HealthSummary::Healthy);
        c.failing += u32::from(h.health == HealthSummary::Failing);
        c.drifted += u32::from(h.drifted);
        c.pending += u32::from(h.status == HostStatus::Pending);
        c.paused += u32::from(h.status == HostStatus::Paused);
    }
    c
}

/// Replaces (or adds) a host in a list.
pub fn apply_hosts(hosts: &mut Vec<HostSummary>, event: &LiveEvent) -> bool {
    let LiveEvent::HostChanged { host } = event else {
        return false;
    };
    match hosts.iter_mut().find(|h| h.id == host.id) {
        Some(existing) => *existing = host.clone(),
        None => hosts.push(host.clone()),
    }
    true
}

pub fn apply_fleet(fleet: &mut Fleet, event: &LiveEvent) {
    match event {
        // A cluster target changes the clusters list; the host counts stay.
        LiveEvent::HostChanged { host } if host.kind == TargetKind::Cluster => {
            fleet.hosts.retain(|h| h.id != host.id);
            if apply_hosts(&mut fleet.clusters, event) {
                fleet.counts = recount(&fleet.hosts);
                fleet.counts.clusters = fleet.clusters.len() as u32;
            }
        }
        LiveEvent::HostChanged { .. } => {
            fleet.clusters.retain(|c| c.id != event_host_id(event));
            if apply_hosts(&mut fleet.hosts, event) {
                fleet.counts = recount(&fleet.hosts);
                fleet.counts.clusters = fleet.clusters.len() as u32;
            }
        }
        // What a cluster is deploying follows its run.
        LiveEvent::RunProgress { progress } => {
            if let Some(status) = fleet
                .clusters
                .iter_mut()
                .find(|c| c.id == progress.host)
                .and_then(|c| c.cluster_status.as_mut())
            {
                super::clusters::track_deploying(status, progress);
            }
        }
        LiveEvent::RolloutChanged { rollout } => {
            if rollout_active(rollout.state) {
                fleet.active_rollout = Some(rollout.id.clone());
            } else if fleet.active_rollout.as_deref() == Some(rollout.id.as_str()) {
                fleet.active_rollout = None;
            }
        }
        _ => {}
    }
}

fn event_host_id(event: &LiveEvent) -> &str {
    match event {
        LiveEvent::HostChanged { host } => host.id.as_str(),
        _ => "",
    }
}

/// A host change replaces the summary; a run update is applied to the
/// latest run the detail carries, when it is that run.
pub fn apply_host_detail(detail: &mut HostDetail, event: &LiveEvent) {
    match event {
        LiveEvent::HostChanged { host } if host.id == detail.summary.id => {
            detail.summary = host.clone();
        }
        LiveEvent::RunProgress { progress } => {
            if let Some(run) = &mut detail.latest_run {
                super::run::apply_progress(run, progress);
            }
            if progress.host == detail.summary.id
                && let Some(status) = &mut detail.summary.cluster_status
            {
                super::clusters::track_deploying(status, progress);
            }
        }
        _ => {}
    }
}

/// Applies a rollout or rollout-host change, or a new timeline entry, to a
/// rollout's detail. A host whose entry now names another phase moves
/// there, so it is still drawn in exactly one stage. A timeline entry the
/// detail already has (the fetch saw it) is not added twice; the timeline
/// stays oldest first.
pub fn apply_rollout(detail: &mut RolloutDetail, event: &LiveEvent) {
    match event {
        LiveEvent::RolloutTimeline { rollout_id, event } if *rollout_id == detail.summary.id => {
            if !detail.events.contains(event) {
                let at = detail
                    .events
                    .iter()
                    .rposition(|e| e.at <= event.at)
                    .map_or(0, |i| i + 1);
                detail.events.insert(at, event.clone());
            }
        }
        LiveEvent::RolloutChanged { rollout } if rollout.id == detail.summary.id => {
            detail.summary = keep_changes(&detail.summary, rollout);
        }
        LiveEvent::RolloutHostChanged { rollout_id, host } if *rollout_id == detail.summary.id => {
            let target = detail.phases.iter().position(|p| p.name == host.phase);
            let current = detail.phases.iter().enumerate().find_map(|(i, p)| {
                p.hosts
                    .iter()
                    .position(|h| super::units::same(h, host))
                    .map(|j| (i, j))
            });
            match (current, target) {
                (Some((i, j)), Some(t)) if i != t => {
                    detail.phases[i].hosts.remove(j);
                    detail.phases[t].hosts.push(host.clone());
                }
                (Some((i, j)), _) => detail.phases[i].hosts[j] = host.clone(),
                (None, Some(t)) => detail.phases[t].hosts.push(host.clone()),
                (None, None) => {}
            }
        }
        _ => {}
    }
}

/// Keeps a list of rollouts current, newest first.
pub fn apply_rollouts(list: &mut Vec<RolloutSummary>, event: &LiveEvent) {
    if let LiveEvent::RolloutChanged { rollout } = event {
        match list.iter_mut().find(|r| r.id == rollout.id) {
            Some(existing) => *existing = keep_changes(existing, rollout),
            None => list.insert(0, rollout.clone()),
        }
    }
}

/// A rollout's update, keeping what it changes: live updates leave the
/// tally out (it takes every host's plan to work out).
fn keep_changes(old: &RolloutSummary, new: &RolloutSummary) -> RolloutSummary {
    RolloutSummary {
        changes: new.changes.or(old.changes),
        ..new.clone()
    }
}

/// Keeps a list of revisions current, newest first.
pub fn apply_revisions(list: &mut Vec<RevisionSummary>, event: &LiveEvent) {
    match event {
        LiveEvent::RevisionAdded { revision } => {
            match list.iter_mut().find(|r| r.id == revision.id) {
                Some(existing) => *existing = revision.clone(),
                None => list.insert(0, revision.clone()),
            }
        }
        // An action rollout runs under a revision without rolling it out.
        LiveEvent::RolloutChanged { rollout } if rollout.action.is_none() => {
            if let Some(r) = list.iter_mut().find(|r| r.id == rollout.revision) {
                r.rollout_id = Some(rollout.id.clone());
            }
        }
        _ => {}
    }
}

/// What the event stream named an event (`event: host_changed`), decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Event(Box<LiveEvent>),
    /// The server dropped events for this client: everything must be refetched.
    Lagged,
    /// The session the stream was opened with no longer passes (expired,
    /// signed out, or refused by the ACLs as they are now); the server has
    /// closed the stream.
    SessionEnded,
}

/// Decodes one SSE frame. The payload's own `type` tag decides the variant;
/// the event name only distinguishes the out-of-band `lagged` and
/// `session_ended` signals.
pub fn decode(name: &str, data: &str) -> Result<Frame, String> {
    match name {
        "lagged" => return Ok(Frame::Lagged),
        "session_ended" => return Ok(Frame::SessionEnded),
        _ => {}
    }
    serde_json::from_str::<LiveEvent>(data)
        .map(|e| Frame::Event(Box::new(e)))
        .map_err(|e| format!("unreadable {name} event: {e}"))
}

/// The event names the server sends, each of which must be subscribed to by
/// name (an `EventSource` only delivers unnamed events to `message`).
pub const EVENT_NAMES: &[&str] = &[
    "host_changed",
    "rollout_changed",
    "rollout_host_changed",
    "rollout_timeline",
    "revision_added",
    "run_progress",
    "audit",
    "lagged",
    "session_ended",
    "message",
];

/// A bounded, sequence-numbered log of the events received, so that a page
/// can ask for everything after the moment it fetched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventLog {
    /// The sequence number of the newest event (0 before the first).
    pub seq: u64,
    entries: VecDeque<(u64, Rc<LiveEvent>)>,
}

impl EventLog {
    pub const CAPACITY: usize = 256;

    pub fn push(&mut self, event: LiveEvent) {
        self.seq += 1;
        self.entries.push_back((self.seq, Rc::new(event)));
        while self.entries.len() > Self::CAPACITY {
            self.entries.pop_front();
        }
    }

    /// The sequence number just before the oldest event still held: what
    /// [`EventLog::since`] takes to return everything it has.
    pub fn oldest(&self) -> u64 {
        self.entries
            .front()
            .map_or(self.seq, |(seq, _)| seq.saturating_sub(1))
    }

    /// The events after `seq`, oldest first, or `None` when some of them have
    /// already been dropped and the caller must fetch afresh.
    pub fn since(&self, seq: u64) -> Option<Vec<Rc<LiveEvent>>> {
        if seq >= self.seq {
            return Some(Vec::new());
        }
        let oldest = self
            .entries
            .front()
            .map(|(s, _)| *s)
            .unwrap_or(self.seq + 1);
        if seq + 1 < oldest {
            return None;
        }
        Some(
            self.entries
                .iter()
                .filter(|(s, _)| *s > seq)
                .map(|(_, e)| e.clone())
                .collect(),
        )
    }
}

/// Exponential reconnect delays with a cap, so a server that is down is not
/// hammered and one that comes back is found again quickly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    pub base_ms: u32,
    pub cap_ms: u32,
    attempt: u32,
}

impl Backoff {
    pub fn new(base_ms: u32, cap_ms: u32) -> Self {
        Self {
            base_ms,
            cap_ms,
            attempt: 0,
        }
    }

    /// The next delay. `jitter` in `0.0..1.0` spreads reconnecting clients up
    /// to a fifth either side of the nominal delay.
    pub fn next_delay(&mut self, jitter: f64) -> u32 {
        let nominal = (self.base_ms as u64)
            .saturating_mul(1u64 << self.attempt.min(16))
            .min(self.cap_ms as u64) as f64;
        self.attempt = self.attempt.saturating_add(1);
        let spread = nominal * 0.2 * (jitter.clamp(0.0, 1.0) * 2.0 - 1.0);
        (nominal + spread).round().max(0.0) as u32
    }

    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{
        HostRolloutState, PhaseState, RevisionStatus, RolloutHost, RolloutPhase, RolloutState,
    };
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn host(id: &str, health: HealthSummary) -> HostSummary {
        HostSummary {
            id: id.into(),
            kind: Default::default(),
            cluster: None,
            status: HostStatus::Active,
            connection: Connection::Online,
            agent_labels: BTreeMap::new(),
            labels: BTreeMap::new(),
            effective_labels: BTreeMap::new(),
            groups: vec![],
            platform: None,
            arch: None,
            agent_version: None,
            applied_revision: None,
            health,
            health_at: None,
            drifted: false,
            drift_at: None,
            first_seen: Utc::now(),
            last_seen: None,
            notes: String::new(),
            upgrade: None,
            cluster_status: None,
        }
    }

    fn summary(id: &str, state: RolloutState) -> RolloutSummary {
        RolloutSummary {
            id: id.into(),
            name: None,
            revision: "rev".into(),
            revision_message: "m".into(),
            state,
            started_at: Utc::now(),
            finished_at: None,
            triggered_by: "controller".into(),
            hosts_total: 2,
            hosts_done: 0,
            hosts_failed: 0,
            current_phase: None,
            pause_reason: None,
            end_reason: None,
            superseded_by: None,
            trace_id: None,
            trace_url: None,
            action: None,
            changes: None,
        }
    }

    fn entry(host: &str, phase: &str, state: HostRolloutState) -> RolloutHost {
        RolloutHost {
            host: host.into(),
            phase: phase.into(),
            wave: None,
            state,
            plan_digest: None,
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            reboot: None,
            kind: Default::default(),
            step: 0,
            workload: None,
            executor: None,
            bake: None,
            preview: false,
            drifted_from: None,
            removal: false,
            earlier: vec![],
            total: None,
            needs_approval: false,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        }
    }

    fn phase(name: &str, hosts: Vec<RolloutHost>) -> RolloutPhase {
        RolloutPhase {
            name: name.into(),
            state: PhaseState::Running,
            parallel: "1".into(),
            approval: "auto".into(),
            bake_seconds: 0,
            max_failures: "0".into(),
            virtual_stage: false,
            hosts,
            approved_by: None,
            approved_at: None,
            bake_until: None,
            health_checks: vec![],
            stages: 1,
            unchanged: 0,
        }
    }

    #[test]
    fn a_host_change_replaces_the_host_and_recounts_the_fleet() {
        let mut fleet = Fleet {
            hosts: vec![
                host("pi-1", HealthSummary::Healthy),
                host("pi-2", HealthSummary::Healthy),
            ],
            clusters: vec![],
            stable_revision: None,
            active_rollout: None,
            counts: FleetCounts::default(),
            server_version: None,
            desired_agent_version: None,
        };
        let event = LiveEvent::HostChanged {
            host: host("pi-2", HealthSummary::Failing),
        };
        apply_fleet(&mut fleet, &event);
        apply_fleet(&mut fleet, &event); // twice is the same as once
        assert_eq!(fleet.hosts.len(), 2);
        assert_eq!((fleet.counts.healthy, fleet.counts.failing), (1, 1));

        apply_fleet(
            &mut fleet,
            &LiveEvent::HostChanged {
                host: host("pi-3", HealthSummary::Unknown),
            },
        );
        assert_eq!(fleet.counts.total, 3);

        // A cluster target lands in the clusters list, not among the hosts.
        let mut cluster = host("home-nomad", HealthSummary::Unknown);
        cluster.kind = TargetKind::Cluster;
        apply_fleet(&mut fleet, &LiveEvent::HostChanged { host: cluster });
        assert_eq!(fleet.counts.total, 3);
        assert_eq!(fleet.counts.clusters, 1);
        assert_eq!(fleet.clusters[0].id, "home-nomad");
    }

    #[test]
    fn the_active_rollout_follows_rollout_transitions() {
        let mut fleet = Fleet {
            hosts: vec![],
            clusters: vec![],
            stable_revision: None,
            active_rollout: None,
            counts: FleetCounts::default(),
            server_version: None,
            desired_agent_version: None,
        };
        apply_fleet(
            &mut fleet,
            &LiveEvent::RolloutChanged {
                rollout: summary("42", RolloutState::Rolling),
            },
        );
        assert_eq!(fleet.active_rollout.as_deref(), Some("42"));
        // Another rollout finishing does not clear this one.
        apply_fleet(
            &mut fleet,
            &LiveEvent::RolloutChanged {
                rollout: summary("41", RolloutState::Completed),
            },
        );
        assert_eq!(fleet.active_rollout.as_deref(), Some("42"));
        // A newer commit's rollout queued behind it does not take its place.
        apply_fleet(
            &mut fleet,
            &LiveEvent::RolloutChanged {
                rollout: summary("43", RolloutState::Detected),
            },
        );
        assert_eq!(fleet.active_rollout.as_deref(), Some("42"));
        apply_fleet(
            &mut fleet,
            &LiveEvent::RolloutChanged {
                rollout: summary("42", RolloutState::Completed),
            },
        );
        assert_eq!(fleet.active_rollout, None);
    }

    #[test]
    fn a_rollout_host_change_updates_in_place_or_moves_stage() {
        let mut detail = RolloutDetail {
            summary: summary("42", RolloutState::Rolling),
            phases: vec![
                phase(
                    "cloud",
                    vec![entry("az-1", "cloud", HostRolloutState::PlanPending)],
                ),
                phase("stragglers", vec![]),
            ],
            plans: vec![],
            fleet: None,
            events: vec![],
        };
        apply_rollout(
            &mut detail,
            &LiveEvent::RolloutHostChanged {
                rollout_id: "42".into(),
                host: entry("az-1", "cloud", HostRolloutState::Planned),
            },
        );
        assert_eq!(detail.phases[0].hosts[0].state, HostRolloutState::Planned);

        apply_rollout(
            &mut detail,
            &LiveEvent::RolloutHostChanged {
                rollout_id: "42".into(),
                host: entry("az-1", "stragglers", HostRolloutState::Pending),
            },
        );
        assert!(detail.phases[0].hosts.is_empty());
        assert_eq!(detail.phases[1].hosts.len(), 1);

        // Another rollout's hosts are not this rollout's business.
        apply_rollout(
            &mut detail,
            &LiveEvent::RolloutHostChanged {
                rollout_id: "41".into(),
                host: entry("pi-9", "cloud", HostRolloutState::Failed),
            },
        );
        assert!(detail.phases[0].hosts.is_empty());

        let mut paused = summary("42", RolloutState::Paused);
        paused.pause_reason = Some("pi-5 failed nomad/api".into());
        apply_rollout(&mut detail, &LiveEvent::RolloutChanged { rollout: paused });
        assert_eq!(detail.summary.state, RolloutState::Paused);
    }

    #[test]
    fn timeline_entries_are_appended_once_in_time_order() {
        let at = |secs: i64| Utc::now() - chrono::Duration::seconds(secs);
        let ev = |kind: &str, when| buckle_api::RolloutEvent {
            at: when,
            kind: kind.into(),
            message: kind.into(),
            host: None,
            workload: None,
            phase: None,
            actor: Some("controller".into()),
        };
        let started = ev("rollout.started", at(60));
        let mut detail = RolloutDetail {
            summary: summary("42", RolloutState::Rolling),
            phases: vec![],
            plans: vec![],
            fleet: None,
            events: vec![started.clone()],
        };
        let live = |rollout: &str, event: &buckle_api::RolloutEvent| LiveEvent::RolloutTimeline {
            rollout_id: rollout.into(),
            event: event.clone(),
        };
        let done = ev("rollout.phase.done", at(10));
        apply_rollout(&mut detail, &live("42", &done));
        apply_rollout(&mut detail, &live("42", &done)); // twice is once
        apply_rollout(&mut detail, &live("42", &started)); // the fetch had it
        apply_rollout(&mut detail, &live("41", &ev("rollout.aborted", at(5))));
        // A late arrival lands where its time puts it.
        let approved = ev("rollout.approve", at(30));
        apply_rollout(&mut detail, &live("42", &approved));
        let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["rollout.started", "rollout.approve", "rollout.phase.done"]
        );
    }

    #[test]
    fn lists_gain_new_items_at_the_top() {
        let mut rollouts = vec![summary("41", RolloutState::Completed)];
        apply_rollouts(
            &mut rollouts,
            &LiveEvent::RolloutChanged {
                rollout: summary("42", RolloutState::Planning),
            },
        );
        apply_rollouts(
            &mut rollouts,
            &LiveEvent::RolloutChanged {
                rollout: summary("42", RolloutState::Rolling),
            },
        );
        assert_eq!(rollouts.len(), 2);
        assert_eq!(
            (rollouts[0].id.as_str(), rollouts[0].state),
            ("42", RolloutState::Rolling)
        );

        let revision = |id: &str| RevisionSummary {
            id: id.into(),
            commit: id.into(),
            author: "a".into(),
            message: "m".into(),
            created_at: Utc::now(),
            status: RevisionStatus::Valid,
            validation_errors: vec![],
            is_stable: false,
            rollout_id: None,
            module_count: 1,
            workload_count: 0,
        };
        let mut revisions = vec![revision("rev")];
        apply_revisions(
            &mut revisions,
            &LiveEvent::RevisionAdded {
                revision: revision("new"),
            },
        );
        apply_revisions(
            &mut revisions,
            &LiveEvent::RolloutChanged {
                rollout: summary("43", RolloutState::Planning),
            },
        );
        let mut action = summary("44", RolloutState::Planning);
        action.action = Some(buckle_api::RolloutActionInfo {
            module: "m".into(),
            action: "a".into(),
            label: "A".into(),
            selector: None,
            instances: None,
        });
        apply_revisions(&mut revisions, &LiveEvent::RolloutChanged { rollout: action });
        assert_eq!(revisions[0].id, "new");
        assert_eq!(revisions[1].rollout_id.as_deref(), Some("43"));
    }

    #[test]
    fn a_run_update_reaches_the_hosts_latest_run() {
        use buckle_api::{RunProgress, RunState, StepState};
        let mut report = super::super::run::tests::report(vec![super::super::run::tests::module(
            "nomad",
            buckle_api::ModuleState::Running,
            vec![super::super::run::tests::step("a", StepState::Running)],
        )]);
        report.host = "pi-1".into();
        let mut detail = HostDetail {
            summary: host("pi-1", HealthSummary::Healthy),
            facts: serde_json::Value::Null,
            facts_at: None,
            resources: vec![],
            recent_runs: vec![],
            health_results: vec![],
            health_checks: vec![],
            drift: vec![],
            latest_run: Some(report),
            workloads: vec![],
            executors: vec![],
            actions: vec![],
        };
        let event = LiveEvent::RunProgress {
            progress: RunProgress {
                host: "pi-1".into(),
                run_id: "run-1".into(),
                rollout_id: None,
                at: Utc::now(),
                run_state: RunState::Running,
                module: Some("nomad".into()),
                module_state: None,
                step: Some("file.copy[a]".into()),
                step_state: Some(StepState::Changed),
                result: None,
                error: None,
            },
        };
        apply_host_detail(&mut detail, &event);
        let run = detail.latest_run.as_ref().unwrap();
        assert_eq!(run.modules[0].steps[0].state, StepState::Changed);
        assert_eq!(run.counts.steps.changed, 1);

        let data = serde_json::to_string(&event).unwrap();
        assert!(EVENT_NAMES.contains(&"run_progress"));
        match decode("run_progress", &data) {
            Ok(Frame::Event(e)) => assert!(matches!(*e, LiveEvent::RunProgress { .. })),
            other => panic!("expected run progress, got {other:?}"),
        }
    }

    #[test]
    fn frames_decode_by_their_payload_tag() {
        let data = serde_json::to_string(&LiveEvent::HostChanged {
            host: host("pi-1", HealthSummary::Healthy),
        })
        .unwrap();
        match decode("host_changed", &data) {
            Ok(Frame::Event(e)) => assert!(matches!(*e, LiveEvent::HostChanged { .. })),
            other => panic!("expected a host change, got {other:?}"),
        }
        assert_eq!(decode("lagged", "{}"), Ok(Frame::Lagged));
        assert_eq!(decode("session_ended", "{}"), Ok(Frame::SessionEnded));
        assert!(EVENT_NAMES.contains(&"session_ended"));
        assert!(decode("host_changed", "{not json").is_err());
    }

    #[test]
    fn the_log_says_when_a_page_has_missed_events() {
        let mut log = EventLog::default();
        let ev = || LiveEvent::HostChanged {
            host: host("pi-1", HealthSummary::Healthy),
        };
        assert_eq!(log.since(0).map(|v| v.len()), Some(0));
        log.push(ev());
        log.push(ev());
        assert_eq!(log.since(0).map(|v| v.len()), Some(2));
        assert_eq!(log.since(1).map(|v| v.len()), Some(1));
        assert_eq!(log.since(2).map(|v| v.len()), Some(0));
        for _ in 0..EventLog::CAPACITY {
            log.push(ev());
        }
        assert_eq!(log.since(1), None, "events 2 and 3 were dropped");
        assert_eq!(
            log.since(log.oldest()).map(|v| v.len()),
            Some(EventLog::CAPACITY),
            "everything still held"
        );
        assert_eq!(log.since(log.seq - 1).map(|v| v.len()), Some(1));
    }

    #[test]
    fn reconnects_back_off_exponentially_up_to_the_cap() {
        let mut b = Backoff::new(1_000, 30_000);
        let delays: Vec<u32> = (0..7).map(|_| b.next_delay(0.5)).collect();
        assert_eq!(
            delays,
            vec![1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
        );
        assert_eq!(b.attempts(), 7);
        b.reset();
        assert_eq!(b.next_delay(0.0), 800);
        assert_eq!(b.next_delay(1.0), 2_400);
    }
}
