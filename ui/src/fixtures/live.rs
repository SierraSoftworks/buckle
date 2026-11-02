//! `?demo&live`: the active rollout moves on by itself, through the same
//! event path a server's `GET /events` stream feeds, so the live updates can
//! be watched without a server.

use buckle_api::{HostRolloutState, LiveEvent, RolloutEvent};
use chrono::{Duration, Utc};

use super::store::{timeline, with};

/// One step of the story: the applying host starts baking, a baking host
/// whose window has passed turns healthy.
pub fn tick() -> Vec<LiveEvent> {
    with(|s| {
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
                    HostRolloutState::Applying => {
                        let secs = phase.bake_seconds.min(30);
                        h.bake_until = Some(now + Duration::seconds(secs as i64));
                        Some((
                            HostRolloutState::Baking,
                            "host.bake_started",
                            format!("Applied; baking for {secs}s."),
                        ))
                    }
                    HostRolloutState::Baking if h.bake_until.is_none_or(|t| t <= now) => {
                        h.bake_until = None;
                        Some((
                            HostRolloutState::Healthy,
                            "host.healthy",
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
