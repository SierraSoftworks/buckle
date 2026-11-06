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

/// The summary row of a target: a host, or (with `kind: cluster` and
/// `cluster` set) a cluster target that has no agent of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSummary {
    pub id: String,
    /// `host` from servers that predate cluster targets.
    #[serde(default)]
    pub kind: crate::TargetKind,
    /// The cluster half of a cluster target; absent for a host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<crate::ClusterInfo>,
    /// For a cluster: its executor and workload tally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_status: Option<crate::ClusterStatus>,
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
    /// When the latest drift check reported (for a cluster, of any of its
    /// placements); absent before the first, and from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_at: Option<DateTime<Utc>>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub notes: String,
    /// An agent upgrade the server asked for and the agent has not yet
    /// settled. Cleared when the agent reconnects running `version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<AgentUpgrade>,
}

/// An agent upgrade requested from the server (`POST /hosts/{id}/upgrade`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUpgrade {
    /// The release the agent was asked to install.
    pub version: String,
    pub state: UpgradeState,
    pub requested_at: DateTime<Utc>,
    pub requested_by: String,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeState {
    /// Sent; the agent installs it once idle, then restarts.
    Requested,
    Failed,
}

/// `POST /api/v1/hosts/{id}/upgrade`. Without a version the agent is moved to
/// the fleet's desired agent version (`agents.desired_version`, or else the
/// server's own).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    /// Each check the host runs now: its latest reading and how it fared in
    /// the host's recent rollouts. Empty from servers that predate it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_checks: Vec<HostCheck>,
    /// What the latest drift check found different from the applied revision,
    /// one entry per drifted resource. Empty when the host has not drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<DriftItem>,
    /// The newest run on this host, module by module and step by step (live
    /// while it is applying). `GET /hosts/{id}/runs/{run}` serves older ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<crate::run::RunReport>,
    /// For a cluster: the workloads its revision selects for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workloads: Vec<crate::WorkloadView>,
    /// For a cluster: the host agents able to speak for it (run its
    /// workloads and operations), active hosts its `executors` selector
    /// matches or its in-cluster executors, sorted. Empty for a host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executors: Vec<String>,
    /// What an operator can run on the target now, from its applied
    /// revision: each selected module's `actions:` (a cluster's: each
    /// selected workload's implicit and extra actions), in module order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionInfo>,
}

/// An action an operator can run on a target
/// (`POST /hosts/{id}/actions/{module}/{name}`, with an
/// [`crate::ActionRequest`] note for the audit log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionInfo {
    /// The module (or workload) declaring it.
    pub module: String,
    pub name: String,
    /// What the UI shows: the action's `name:`, else its key.
    pub label: String,
    /// The task kind it runs (`apt.upgrade`, `dispatch`, `restart` for an
    /// implicit workload action).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ask the operator to confirm, with a note for the audit log.
    #[serde(default)]
    pub confirm: bool,
    /// An implicit workload action (restart, stop, roll back, a Nomad
    /// task's `action` block).
    #[serde(default)]
    pub implicit: bool,
    /// Set where the operator may pick which of the workload's instances it
    /// runs in (a Nomad task action whose group may run several
    /// allocations): the default. Absent: it runs where it always does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<ActionInstances>,
}

/// Which of a workload's instances (a Nomad task group's running
/// allocations) an action runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionInstances {
    /// One of them, picked by Buckle.
    #[default]
    Any,
    /// Each of them, one after another.
    All,
}

impl ActionInstances {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionInstances::Any => "any",
            ActionInstances::All => "all",
        }
    }
}

/// An action and the targets offering it now
/// (`GET /api/v1/actions?selector=…`), for an action rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetAction {
    pub action: ActionInfo,
    /// A workload's action, offered by the clusters running it; else a host
    /// module's.
    #[serde(default)]
    pub workload: bool,
    /// The targets offering it (that the selector matches), by name.
    pub targets: Vec<String>,
}

/// The run an action request started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionStarted {
    /// The run's id (`GET /hosts/{id}/runs/{run}`).
    pub run_id: String,
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
    /// Stable identifier, the key for `GET /hosts/{id}/runs/{run}`. Empty from
    /// servers that predate run reports.
    #[serde(default)]
    pub id: String,
    /// The run's state; absent from servers that predate run reports (use
    /// `outcome`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::run::RunState>,
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
    /// For an operator's action, `module/action`; absent for an apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
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

/// One check on one host, aggregated: what it reads now and, per recent
/// rollout, how it fared while that rollout's revision was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCheck {
    pub module: String,
    pub check: String,
    pub kind: String,
    pub required: bool,
    /// The latest reading: `pass`, `fail` or `unknown`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The scope of the latest reading (`drift`, `bake`, …).
    pub scope: String,
    pub at: DateTime<Utc>,
    /// Newest first, one entry per recent rollout the check ran under.
    #[serde(default)]
    pub history: Vec<CheckEpoch>,
}

/// How one check fared while one rollout's revision was applied on a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckEpoch {
    /// Absent for applies outside a rollout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    pub revision: String,
    /// The last reading in the epoch.
    pub status: String,
    /// The worst reading in the epoch (`fail` over `unknown` over `pass`).
    pub worst: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub readings: u32,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

/// `PATCH /api/v1/hosts/{id}` and `PATCH /api/v1/targets/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// A new spec for a cluster target an operator created; rejected for a
    /// host and for a cluster `buckle.yaml` declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<crate::ClusterSpec>,
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
    /// Cluster targets, declared in `buckle.yaml` or created through the API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<HostSummary>,
    pub stable_revision: Option<String>,
    pub active_rollout: Option<String>,
    pub counts: FleetCounts,
    /// The server's own version, which agents converge to on self-update; the
    /// UI flags agents reporting a different one as version skew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// The agent version the fleet should run: `agents.desired_version`, or
    /// else the server's own. What an upgrade without a version installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_agent_version: Option<String>,
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
    /// Cluster targets; not part of `total`, which counts hosts.
    #[serde(default)]
    pub clusters: u32,
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
        assert!(detail.health_checks.is_empty());

        let fleet: Fleet = serde_json::from_value(json!({
            "hosts": [summary], "stable_revision": null, "active_rollout": null,
            "counts": FleetCounts::default()
        }))
        .unwrap();
        assert_eq!(fleet.server_version, None);
        assert_eq!(fleet.hosts[0].kind, TargetKind::Host);
        assert_eq!(fleet.hosts[0].cluster, None);
        assert!(fleet.clusters.is_empty());

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
