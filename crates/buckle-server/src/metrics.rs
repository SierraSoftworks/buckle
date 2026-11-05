//! The server's OpenTelemetry gauges (design §13), sampled from the store
//! and the session registry every [`INTERVAL`]:
//!
//! * `buckle.rollout.hosts` — hosts of live rollouts, by `buckle.rollout`,
//!   `buckle.phase` and `state` (the host's rollout state);
//! * `buckle.agent.connected` — agents with an open session.
//!
//! Instruments come from the global meter provider, which tracing-batteries
//! installs (with `OpenTelemetry::with_metrics`) when an OTLP endpoint is
//! configured; without one they are no-ops. A gauge series that disappears
//! (a rollout finished, a host moved on) is recorded once more as 0 so
//! dashboards do not keep showing its last value.

use crate::rollout::Sessions;
use crate::store::Store;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::opentelemetry::{KeyValue, global};

/// How often the gauges are sampled.
pub const INTERVAL: Duration = Duration::from_secs(15);

/// `(rollout, phase, state)` → number of hosts.
pub type RolloutHosts = BTreeMap<(String, String, String), u64>;

/// Counts the hosts of every live rollout by phase and state.
pub async fn rollout_hosts(store: &Store) -> buckle_core::Result<RolloutHosts> {
    store
        .call(|db| {
            let mut out = RolloutHosts::new();
            for rollout in db.rollouts().live()? {
                for host in db.rollouts().hosts(&rollout.id)? {
                    *out.entry((
                        rollout.id.clone(),
                        host.phase.clone(),
                        host.state.as_str().to_owned(),
                    ))
                    .or_default() += 1;
                }
            }
            Ok(out)
        })
        .await
}

/// Records the gauges every [`INTERVAL`] until `cancel` trips.
pub async fn run(store: Store, sessions: Arc<Sessions>, cancel: CancellationToken) {
    let meter = global::meter("buckle");
    let hosts = meter
        .u64_gauge("buckle.rollout.hosts")
        .with_description("Hosts of live rollouts, by rollout, phase and state.")
        .build();
    let connected = meter
        .u64_gauge("buckle.agent.connected")
        .with_description("Agents with an open session to this server.")
        .build();
    let mut previous = RolloutHosts::new();
    loop {
        connected.record(sessions.connected().len() as u64, &[]);
        match rollout_hosts(&store).await {
            Ok(current) => {
                for key in previous.keys().filter(|k| !current.contains_key(*k)) {
                    hosts.record(0, &attributes(key));
                }
                for (key, n) in &current {
                    hosts.record(*n, &attributes(key));
                }
                previous = current;
            }
            Err(e) => tracing::debug!(error = %e, "rollout gauges could not be sampled"),
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(INTERVAL) => {}
        }
    }
}

fn attributes((rollout, phase, state): &(String, String, String)) -> [KeyValue; 3] {
    [
        KeyValue::new("buckle.rollout", rollout.clone()),
        KeyValue::new("buckle.phase", phase.clone()),
        KeyValue::new("state", state.clone()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::model::{HostPhase, HostRuntime, RolloutRecord, RolloutRuntime};
    use crate::store::RolloutHostRecord;
    use buckle_api::RolloutState;

    fn row(rollout: &str, host: &str, phase: &str, state: HostPhase) -> RolloutHostRecord {
        RolloutHostRecord {
            rollout: rollout.into(),
            host: host.into(),
            workload: String::new(),
            phase: phase.into(),
            wave: None,
            state,
            plan_digest: None,
            plan: None,
            result: None,
            health: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            runtime: HostRuntime::default(),
        }
    }

    fn rollout(id: &str, state: RolloutState) -> RolloutRecord {
        RolloutRecord {
            id: id.into(),
            revision: "r".into(),
            state,
            policy: Default::default(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            triggered_by: "controller".into(),
            runtime: RolloutRuntime::default(),
        }
    }

    #[tokio::test]
    async fn live_rollout_hosts_are_counted_by_phase_and_state() {
        let store = Store::in_memory().await.unwrap();
        store
            .call(|db| {
                db.rollouts().save(&rollout("ro", RolloutState::Rolling))?;
                db.rollouts()
                    .save_host(&row("ro", "a", "canary", HostPhase::Healthy))?;
                db.rollouts()
                    .save_host(&row("ro", "b", "fleet", HostPhase::Applying))?;
                db.rollouts()
                    .save_host(&row("ro", "c", "fleet", HostPhase::Applying))?;
                db.rollouts()
                    .save(&rollout("done", RolloutState::Aborted))?;
                db.rollouts()
                    .save_host(&row("done", "a", "fleet", HostPhase::Failed))?;
                Ok(())
            })
            .await
            .unwrap();
        let counts = rollout_hosts(&store).await.unwrap();
        let key = |p: &str, s: &str| ("ro".to_string(), p.to_string(), s.to_string());
        assert_eq!(counts.len(), 2, "{counts:?}");
        assert_eq!(counts[&key("canary", "healthy")], 1);
        assert_eq!(counts[&key("fleet", "applying")], 2);
    }

    #[tokio::test]
    async fn recording_without_a_meter_provider_is_harmless() {
        let store = Store::in_memory().await.unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(store, Arc::new(Sessions::new()), cancel.clone()));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
