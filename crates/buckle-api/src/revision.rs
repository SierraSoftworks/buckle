use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionStatus {
    Valid,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionSummary {
    pub id: String,
    pub commit: String,
    pub author: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub status: RevisionStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<String>,
    pub is_stable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    pub module_count: u32,
    /// Workloads under `workloads/`; 0 from servers that predate it.
    #[serde(default)]
    pub workload_count: u32,
}

/// `POST /api/v1/revisions/{id}/rollout`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRollout {
    #[serde(default)]
    pub note: String,
}
