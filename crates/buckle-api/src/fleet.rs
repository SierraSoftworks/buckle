use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    /// Enrolled but awaiting an admin's approval.
    Pending,
    Active,
    /// Kept in the fleet but excluded from rollouts and drift correction.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Online,
    Offline,
}

/// Health as the server last heard it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSummary {
    Healthy,
    Failing,
    Held,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSummary {
    pub id: String,
    pub status: HostStatus,
    pub connection: Connection,
    /// Seed labels reported by the agent.
    pub agent_labels: BTreeMap<String, String>,
    /// Labels set by operators; win on conflict.
    pub labels: BTreeMap<String, String>,
    /// The merged view.
    pub effective_labels: BTreeMap<String, String>,
    pub groups: Vec<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub agent_version: Option<String>,
    pub applied_revision: Option<String>,
    pub health: HealthSummary,
    /// When the health reading was taken.
    pub health_at: Option<DateTime<Utc>>,
    pub drifted: bool,
    pub first_seen: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostDetail {
    #[serde(flatten)]
    pub summary: HostSummary,
    /// The facts tree.
    pub facts: serde_json::Value,
    pub facts_at: Option<DateTime<Utc>>,
    pub resources: Vec<ResourceSummary>,
    pub recent_runs: Vec<RunSummary>,
    pub health_results: Vec<HealthResultDto>,
    /// What the latest drift check found different from the applied revision,
    /// one entry per drifted resource. Empty when the host has not drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<DriftItem>,
}

/// One resource whose live state no longer matches what Buckle applied, as
/// the drift check (design §04, §07) reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftItem {
    pub module: String,
    pub kind: String,
    pub key: String,
    /// The task's own description of the difference (`1 → 60`, `mode 0644 → 0600`).
    pub summary: String,
    /// A unified diff of redacted shadows (applied vs current), when the kind
    /// can produce one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSummary {
    pub module: String,
    pub kind: String,
    pub key: String,
    pub revision: String,
    pub applied_at: DateTime<Utc>,
    pub removal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub revision: String,
    pub plan_digest: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub outcome: String,
    pub changes: u32,
    pub removals: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// A link to the run's trace in the configured tracing UI, when the
    /// server knows one (otherwise the UI shows `trace_id` as text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResultDto {
    pub scope: String,
    pub module: String,
    pub check: String,
    pub kind: String,
    pub required: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
    /// The rollout the check ran for (`baseline`, `bake` and `rollout`
    /// scopes); absent for `drift` readings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
}

/// `PATCH /api/v1/hosts/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// `POST /api/v1/hosts/{id}/preview` → which groups and phase a label set yields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPreview {
    pub effective_labels: BTreeMap<String, String>,
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// `GET /api/v1/fleet`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fleet {
    pub hosts: Vec<HostSummary>,
    pub stable_revision: Option<String>,
    pub active_rollout: Option<String>,
    pub counts: FleetCounts,
    /// The server's own version, which agents converge to on self-update; the
    /// UI flags agents reporting a different one as version skew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCounts {
    pub total: u32,
    pub online: u32,
    pub healthy: u32,
    pub failing: u32,
    pub drifted: u32,
    pub pending: u32,
    pub paused: u32,
}

#[cfg(test)]
mod tests {
    use crate::*;
    use serde_json::json;

    /// The fields the UI lane added are optional on the wire, so a server that
    /// predates them (or chooses not to fill them) still deserialises.
    #[test]
    fn fields_added_for_the_ui_default_when_absent() {
        let summary = json!({
            "id": "pi-1", "status": "active", "connection": "online",
            "agent_labels": {}, "labels": {}, "effective_labels": {}, "groups": [],
            "platform": null, "arch": null, "agent_version": null, "applied_revision": null,
            "health": "healthy", "health_at": null, "drifted": false,
            "first_seen": "2026-11-01T00:00:00Z", "last_seen": null
        });
        let mut detail = summary.clone();
        let extra = json!({
            "facts": {}, "facts_at": null, "resources": [],
            "recent_runs": [{
                "revision": "r", "plan_digest": "d",
                "started_at": "2026-11-01T00:00:00Z", "finished_at": "2026-11-01T00:01:00Z",
                "outcome": "success", "changes": 1, "removals": 0
            }],
            "health_results": [{
                "scope": "drift", "module": "m", "check": "c", "kind": "http",
                "required": true, "status": "pass", "at": "2026-11-01T00:00:00Z"
            }]
        });
        for (k, v) in extra.as_object().unwrap() {
            detail[k] = v.clone();
        }
        let detail: HostDetail = serde_json::from_value(detail).unwrap();
        assert!(detail.drift.is_empty());
        assert_eq!(detail.recent_runs[0].trace_url, None);
        assert_eq!(detail.health_results[0].rollout_id, None);

        let fleet: Fleet = serde_json::from_value(json!({
            "hosts": [summary], "stable_revision": null, "active_rollout": null,
            "counts": FleetCounts::default()
        }))
        .unwrap();
        assert_eq!(fleet.server_version, None);

        let rollout: RolloutSummary = serde_json::from_value(json!({
            "id": "42", "revision": "r", "revision_message": "m", "state": "rolling",
            "started_at": "2026-11-01T00:00:00Z", "triggered_by": "controller",
            "hosts_total": 1, "hosts_done": 0, "hosts_failed": 0
        }))
        .unwrap();
        assert_eq!(rollout.trace_url, None);
    }

    #[test]
    fn drift_items_round_trip_and_stay_off_the_wire_when_empty() {
        let item = DriftItem {
            module: "common".into(),
            kind: "sysctl.param".into(),
            key: "vm.swappiness".into(),
            summary: "1 → 60".into(),
            unified: None,
            detected_at: None,
        };
        let text = serde_json::to_string(&item).unwrap();
        assert!(!text.contains("unified"));
        assert_eq!(serde_json::from_str::<DriftItem>(&text).unwrap(), item);
    }
}
