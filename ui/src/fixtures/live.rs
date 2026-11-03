//! `?demo&live`: the active rollout moves on by itself, and pi-4's run
//! advances step by step, through the same event path a server's
//! `GET /events` stream feeds, so the live updates can be watched without a
//! server.

use buckle_api::{HostRolloutState, LiveEvent, RolloutEvent};
use chrono::{Duration, Utc};

use super::runs::{LIVE_HOST, live_frame};
use super::store::{timeline, with};
use crate::logic::run::{apply_progress, run_finished};

/// One step of pi-4's run: the updates the agent would send, applied to the
/// demo server's copy of the report first. When the run ends, its summary
/// and the host's applied revision follow.
pub fn run_tick() -> Vec<LiveEvent> {
    with(|s| {
        let frame = s.run_frame;
        let Some(detail) = s.details.iter_mut().find(|d| d.summary.id == LIVE_HOST) else {
            return Vec::new();
        };
        let Some(report) = detail.latest_run.as_mut() else {
            return Vec::new();
        };
        if run_finished(report.state) {
            return Vec::new();
        }
        s.run_frame += 1;
        let updates = live_frame(frame, report);
        for u in &updates {
            apply_progress(report, u);
        }
        let mut events: Vec<LiveEvent> = updates
            .into_iter()
            .map(|progress| LiveEvent::RunProgress { progress })
            .collect();
        if run_finished(report.state) {
            let summary = super::runs::summary_of(report, "success", None);
            let revision = report.revision.clone();
            detail.recent_runs[0] = summary;
            if let Some(host) = s.hosts.iter_mut().find(|h| h.id == LIVE_HOST) {
                host.applied_revision = Some(revision);
                let host = host.clone();
                detail.summary = host.clone();
                events.push(LiveEvent::HostChanged { host });
            }
        }
        events
    })
}

/// One step of the story: an applying host whose run has finished starts
/// baking, a baking host whose window has passed turns healthy.
pub fn tick() -> Vec<LiveEvent> {
    with(|s| {
        let running: Vec<String> = s
            .details
            .iter()
            .filter(|d| {
                d.latest_run
                    .as_ref()
                    .is_some_and(|r| !run_finished(r.state))
            })
            .map(|d| d.summary.id.clone())
            .collect();
        let Ok(detail) = s.rollout_mut("42") else {
            return Vec::new();
        };
        let now = Utc::now();
        let before = detail.events.len();
        let mut changed = Vec::new();
        let mut entries = Vec::new();
        for phase in &mut detail.phases {
            for h in &mut phase.hosts {
                let next = match h.state {
                    HostRolloutState::Applying if !running.contains(&h.host) => {
                        let secs = phase.bake_seconds.min(30);
                        h.bake_until = Some(now + Duration::seconds(secs as i64));
                        Some((
                            HostRolloutState::Baking,
                            "rollout.host.baking",
                            format!("Applied; baking for {secs}s."),
                        ))
                    }
                    HostRolloutState::Baking if h.bake_until.is_none_or(|t| t <= now) => {
                        h.bake_until = None;
                        Some((
                            HostRolloutState::Healthy,
                            "rollout.host.healthy",
                            "Baked with every required check passing.".to_string(),
                        ))
                    }
                    _ => None,
                };
                if let Some((state, kind, message)) = next {
                    h.state = state;
                    h.updated_at = now;
                    changed.push(h.clone());
                    entries.push(RolloutEvent {
                        at: now,
                        kind: kind.into(),
                        message,
                        host: Some(h.host.clone()),
                        phase: Some(phase.name.clone()),
                        actor: Some("controller".into()),
                    });
                }
            }
        }
        detail.events.extend(entries);
        detail.summary.hosts_done = detail
            .phases
            .iter()
            .flat_map(|p| &p.hosts)
            .filter(|h| h.state == HostRolloutState::Healthy)
            .count() as u32;
        let summary = detail.summary.clone();
        let mut events: Vec<LiveEvent> = changed
            .into_iter()
            .map(|host| LiveEvent::RolloutHostChanged {
                rollout_id: "42".into(),
                host,
            })
            .collect();
        if !events.is_empty() {
            events.push(LiveEvent::RolloutChanged { rollout: summary });
        }
        events.extend(timeline(detail, before));
        events
    })
}
