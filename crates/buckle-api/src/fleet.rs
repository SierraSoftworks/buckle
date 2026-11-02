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
