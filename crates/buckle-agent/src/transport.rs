//! The agent's view of its control plane. The gRPC transport talks to a
//! server; the local transport turns directory changes into assignments so
//! the same session loop runs in both modes.

use async_trait::async_trait;
use buckle_core::plan::RevisionId;
use std::path::PathBuf;

/// What the control plane tells the agent to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// Fetch and plan this revision; `stable` marks the fleet's stable revision.
    Assign {
        revision: RevisionId,
        stable: bool,
    },
    RequestPlan {
        revision: RevisionId,
        select: Option<String>,
    },
    Apply {
        revision: RevisionId,
        plan_digest: String,
        waived_checks: Vec<String>,
        force: bool,
    },
    Rollback {
        revision: RevisionId,
        reason: String,
    },
    RunHealth {
        request_id: String,
        scope: buckle_core::health::HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    },
    Upgrade {
        version: String,
    },
    /// The session is being closed by the other side.
    Rejected {
        reason: String,
        retry: bool,
    },
}

/// Where a revision's content comes from.
#[derive(Debug, Clone)]
pub enum RevisionSource {
    Directory(PathBuf),
    Tarball(Vec<u8>),
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Waits for the next instruction. `None` means the transport closed.
    async fn next(&mut self) -> buckle_core::Result<Option<Instruction>>;

    /// Fetches the content of a revision.
    async fn fetch_revision(&self, revision: &RevisionId) -> buckle_core::Result<RevisionSource>;

    /// Sends a report (facts, plan, apply events, results, health, drift, local rollback).
    async fn report(&self, message: Report) -> buckle_core::Result<()>;
}

/// Agent → control plane messages, transport-neutral. The gRPC transport
/// converts these to protobuf; the local transport logs them.
#[derive(Debug, Clone)]
pub enum Report {
    Facts(buckle_core::facts::Facts),
    Plan(buckle_core::plan::Plan),
    PlanFailed {
        revision: RevisionId,
        error: String,
    },
    ApplyEvent {
        revision: RevisionId,
        plan_digest: String,
        event: buckle_core::engine::ApplyEvent,
    },
    ApplyResult {
        revision: RevisionId,
        outcome: buckle_core::engine::ApplyOutcome,
    },
    Health {
        request_id: Option<String>,
        revision: Option<RevisionId>,
        report: buckle_core::health::HealthReport,
    },
    Drift {
        revision: RevisionId,
        plan: Option<buckle_core::plan::Plan>,
        corrected: bool,
    },
    LocalRollback {
        from: RevisionId,
        to: RevisionId,
        reason: String,
        modules: Vec<String>,
    },
}
