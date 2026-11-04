//! The agent's view of its control plane. The gRPC transport talks to a
//! server; the local transport turns directory changes into assignments so
//! the same session loop runs in both modes.
//!
//! A [`Transport`] opens [`Session`]s. A session is a stream of
//! [`Instruction`]s from the control plane and a sink for [`Report`]s back to
//! it; when the instruction channel closes the session is over and the agent
//! reconnects with backoff. Bulk content (revision trees, secret top-ups) is
//! fetched through the transport outside the session.

pub mod convert;
pub mod grpc;
pub mod local;
pub mod tls;

use async_trait::async_trait;
use buckle_core::plan::RevisionId;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// The first message of a session, agent → control plane.
#[derive(Clone, PartialEq, Eq)]
pub struct Hello {
    /// The host identity (agent.yaml `host_id`, else the hostname).
    pub host_id: String,
    /// The agent's version, for the server's minimum-version check.
    pub agent_version: String,
    /// Seed labels from agent.yaml; server labels override them.
    pub labels: IndexMap<String, String>,
    /// The revision currently applied, if any.
    pub applied_revision: Option<RevisionId>,
    /// Digest of the last plan the agent produced, if any.
    pub last_plan_digest: Option<String>,
    /// The per-host token, or the bootstrap token before enrolment.
    pub token: Option<String>,
    /// Operating system (`linux`, `macos`, `windows`).
    pub platform: String,
    /// CPU architecture (`x86_64`, `aarch64`).
    pub arch: String,
    /// Whether `token` is the bootstrap token (first contact).
    pub bootstrap: bool,
    /// Revisions whose apply is running, or whose result is not yet
    /// acknowledged (it follows the Welcome).
    pub unreported_apply_revisions: Vec<RevisionId>,
    /// The identity of the current boot, when the host exposes one.
    pub boot_id: Option<String>,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("host_id", &self.host_id)
            .field("agent_version", &self.agent_version)
            .field("labels", &self.labels)
            .field("applied_revision", &self.applied_revision)
            .field("last_plan_digest", &self.last_plan_digest)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("platform", &self.platform)
            .field("arch", &self.arch)
            .field("bootstrap", &self.bootstrap)
            .field(
                "unreported_apply_revisions",
                &self.unreported_apply_revisions,
            )
            .field("boot_id", &self.boot_id)
            .finish()
    }
}

/// A secret value as delivered by the server, before it is sealed into a
/// [`buckle_core::secrets::Secret`]. Its `Debug` never shows the value, it
/// is not serialisable, and its buffer is zeroed when it is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct WireSecret {
    /// The secret's name as referenced by modules.
    pub name: String,
    /// The raw value.
    pub value: Vec<u8>,
    /// The server's fingerprint of the value, for display.
    pub fingerprint: String,
}

impl Drop for WireSecret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.value);
    }
}

impl std::fmt::Debug for WireSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireSecret")
            .field("name", &self.name)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// The server's answer to [`Hello`]. Its `Debug` hides the issued token.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Welcome {
    /// The server's version.
    pub server_version: String,
    /// The server's identifier for this session.
    pub session_id: String,
    /// The fleet's stable revision, cached for local rollback.
    pub stable_revision: Option<RevisionId>,
    /// A per-host token to persist (enrolment or rotation).
    pub issued_token: Option<String>,
    /// Effective labels after server overrides.
    pub labels: IndexMap<String, String>,
    /// `pending`, `active` or `paused`.
    pub status: String,
    /// Secrets for the stable revision, scoped to this host.
    pub secrets: Vec<WireSecret>,
    /// Digest of the stable revision's tarball, to verify the cached tree.
    pub stable_tree_digest: Option<String>,
}

impl std::fmt::Debug for Welcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Welcome")
            .field("server_version", &self.server_version)
            .field("session_id", &self.session_id)
            .field("stable_revision", &self.stable_revision)
            .field(
                "issued_token",
                &self.issued_token.as_ref().map(|_| "<redacted>"),
            )
            .field("labels", &self.labels)
            .field("status", &self.status)
            .field("secrets", &self.secrets)
            .field("stable_tree_digest", &self.stable_tree_digest)
            .finish()
    }
}

/// What changed about the host, pushed while connected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostUpdate {
    /// `pending`, `active` or `paused`.
    pub status: String,
    /// Effective labels after server overrides.
    pub labels: IndexMap<String, String>,
    /// The fleet's stable revision.
    pub stable_revision: Option<RevisionId>,
    /// Secrets for the stable revision, scoped to this host.
    pub secrets: Vec<WireSecret>,
    /// Digest of the stable revision's tarball.
    pub stable_tree_digest: Option<String>,
}

/// What the control plane tells the agent to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// The session is established; always the first instruction of a session.
    Welcome(Welcome),
    /// Fetch and load this revision; `stable` marks the fleet's stable revision.
    Assign {
        /// The assigned revision.
        revision: RevisionId,
        /// Whether it is the fleet's stable revision.
        stable: bool,
        /// Secrets the revision's selected modules reference.
        secrets: Vec<WireSecret>,
        /// Digest of the revision tarball, for cache validation.
        tree_digest: Option<String>,
        /// The cluster target this concerns; `None` for the host itself.
        target: Option<String>,
    },
    /// Plan the revision and report the plan.
    RequestPlan {
        /// The revision to plan.
        revision: RevisionId,
        /// Optional filt-rs task selector.
        select: Option<String>,
        /// The cluster target to plan for; `None` for the host itself.
        target: Option<String>,
    },
    /// Apply the revision if its fresh plan has the approved digest.
    Apply {
        /// The revision to apply.
        revision: RevisionId,
        /// The approved plan digest, or `None` for an automatic apply (local
        /// mode) or an operator's force apply, which applies whatever the
        /// fresh plan is.
        plan_digest: Option<String>,
        /// The approved intent digest ([`buckle_core::plan::Plan::intent_digest`]).
        /// When set it binds the apply instead of `plan_digest`, so a fresh
        /// plan that differs only in rendered content still applies.
        approval_digest: Option<String>,
        /// Checks waived for this run (`module/check`).
        waived_checks: Vec<String>,
        /// The operator forced the apply despite a failing baseline.
        force: bool,
        /// W3C trace context of the span driving the apply.
        traceparent: Option<String>,
        /// The selector the approved plan was made with.
        select: Option<String>,
        /// The rollout driving the apply, for tracing.
        rollout_id: Option<String>,
        /// The rollout phase, for tracing.
        phase: Option<String>,
        /// The cluster target to apply to; `None` for the host itself.
        target: Option<String>,
    },
    /// Apply this (older) revision now.
    Rollback {
        /// The revision to roll back to.
        revision: RevisionId,
        /// Why, for logs and the run record.
        reason: String,
        /// The rollout being rolled back, for tracing.
        rollout_id: Option<String>,
        /// W3C trace context of the span driving the rollback.
        traceparent: Option<String>,
        /// The cluster target to roll back; `None` for the host itself.
        target: Option<String>,
    },
    /// Run health checks now.
    RunHealth {
        /// Which request the report answers.
        request_id: String,
        /// The scope the checks run in.
        scope: buckle_core::health::HealthScope,
        /// Modules to check; empty means every applied module.
        modules: Vec<String>,
        /// For bake: repeat until this instant.
        until: Option<chrono::DateTime<chrono::Utc>>,
        /// W3C trace context of the requesting span.
        traceparent: Option<String>,
        /// The cluster target to check; `None` for the host itself.
        target: Option<String>,
    },
    /// Keepalive from the server; nothing to do.
    Ping,
    /// Install this agent version when idle, then restart.
    Upgrade {
        /// The release to install.
        version: String,
        /// Wait until no other operation is queued.
        when_idle: bool,
    },
    /// The host's record changed (approved, paused, resumed, relabelled).
    HostUpdate(HostUpdate),
    /// The server committed these durable reports; forget them.
    Ack {
        /// The acknowledged report ids.
        report_ids: Vec<u64>,
    },
    /// The session is being closed by the other side.
    Rejected {
        /// Why the session was refused.
        reason: String,
        /// Whether reconnecting can help.
        retry: bool,
    },
}

impl Instruction {
    /// The cluster target the instruction concerns, when it is not the host
    /// itself. An agent that does not run workloads answers a targeted
    /// RequestPlan with a failure and ignores the rest.
    pub fn target(&self) -> Option<&str> {
        match self {
            Instruction::Assign { target, .. }
            | Instruction::RequestPlan { target, .. }
            | Instruction::Apply { target, .. }
            | Instruction::Rollback { target, .. }
            | Instruction::RunHealth { target, .. } => target.as_deref(),
            _ => None,
        }
    }
}

/// Where a revision's content comes from.
#[derive(Debug, Clone)]
pub enum RevisionSource {
    /// A directory to load in place (local mode).
    Directory(PathBuf),
    /// A verified gzip tarball of `buckle.yaml`, `modules/` and `vars/`.
    Tarball(Vec<u8>),
}

/// An open session.
pub struct Session {
    /// Instructions from the control plane; closing means the session ended.
    pub instructions: mpsc::Receiver<Instruction>,
    /// Reports to the control plane. Sends fail once the session ended.
    pub reports: mpsc::Sender<Report>,
}

/// A control plane the agent can open sessions with.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Opens a session, sending `hello` first. Errors mean the control plane
    /// is unreachable or refused the connection outright.
    async fn connect(&self, hello: Hello) -> buckle_core::Result<Session>;

    /// Fetches the content of a revision. `tree_digest`, when known, is the
    /// digest the content must have.
    async fn fetch_revision(
        &self,
        revision: &RevisionId,
        tree_digest: Option<&str>,
    ) -> buckle_core::Result<RevisionSource>;

    /// Fetches secrets for modules selected after facts changed.
    async fn fetch_secrets(
        &self,
        revision: &RevisionId,
        names: &[String],
    ) -> buckle_core::Result<Vec<WireSecret>>;
}

/// Agent → control plane messages, transport-neutral. The gRPC transport
/// converts these to protobuf; the local transport logs them.
#[derive(Debug, Clone)]
pub enum Report {
    /// The full facts tree (on connect and whenever it changes).
    Facts(buckle_core::facts::Facts),
    /// Liveness.
    Heartbeat {
        /// The revision currently applied.
        applied_revision: Option<RevisionId>,
        /// Whether an operation is running.
        busy: bool,
    },
    /// A fresh plan.
    Plan(buckle_core::plan::Plan),
    /// Planning failed.
    PlanFailed {
        /// The revision that failed to plan.
        revision: RevisionId,
        /// The error, for the operator.
        error: String,
        /// The cluster target the plan was for; `None` for the host itself.
        target: Option<String>,
    },
    /// An Apply was refused because the fresh plan differs from the approved one.
    StalePlan {
        /// The fresh plan, for re-approval.
        plan: buckle_core::plan::Plan,
        /// The digest the operator approved.
        approved_digest: String,
    },
    /// Progress during an apply (coalesced).
    ApplyEvent {
        /// The revision being applied.
        revision: RevisionId,
        /// The digest of the plan being applied.
        plan_digest: String,
        /// When the event happened.
        at: chrono::DateTime<chrono::Utc>,
        /// The event.
        event: buckle_core::engine::ApplyEvent,
    },
    /// The result of an apply (durable: kept until acknowledged).
    ApplyResult {
        /// The durable report id.
        report_id: u64,
        /// The revision that was applied.
        revision: RevisionId,
        /// The outcome.
        outcome: buckle_core::engine::ApplyOutcome,
        /// The revision applied on the host after the run.
        applied_revision: Option<RevisionId>,
    },
    /// Health check results.
    Health {
        /// The RunHealth request this answers, if any.
        request_id: Option<String>,
        /// The revision whose checks ran.
        revision: Option<RevisionId>,
        /// The results.
        report: buckle_core::health::HealthReport,
    },
    /// A periodic drift pass. `plan` is `None` when nothing drifted.
    Drift {
        /// The applied revision that was re-planned.
        revision: RevisionId,
        /// The drift plan, when something changed under us.
        plan: Option<buckle_core::plan::Plan>,
        /// Whether the drift was corrected.
        corrected: bool,
    },
    /// The agent rolled itself (or a module) back on its own (durable).
    LocalRollback(LocalRollbackRecord),
    /// How an Upgrade went.
    Upgrade {
        /// The version the Upgrade asked for.
        version: String,
        /// What happened.
        outcome: UpgradeReport,
    },
    /// The reboot a reported run asked for could not be started.
    RebootFailed {
        /// The boot the host is still on.
        boot_id: Option<String>,
        /// Why, for the operator.
        error: String,
    },
}

/// The outcome of an Upgrade, as reported to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeReport {
    /// Installed; the agent is restarting into it.
    Updated,
    /// Already running the requested version.
    UpToDate,
    /// The upgrade failed.
    Failed(String),
}

impl Report {
    /// The durable report id, for reports the server acknowledges.
    pub fn report_id(&self) -> Option<u64> {
        match self {
            Report::ApplyResult { report_id, .. } => Some(*report_id),
            Report::LocalRollback(record) => Some(record.report_id),
            _ => None,
        }
    }
}

/// A report kept in agent.json until the server acknowledges it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableReport {
    /// An ApplyResult.
    ApplyResult {
        /// The durable report id.
        report_id: u64,
        /// The revision that was applied.
        revision: RevisionId,
        /// The outcome (`buckle_core::engine::ApplyOutcome` as JSON).
        outcome: serde_json::Value,
        /// The revision applied on the host after the run.
        applied_revision: Option<RevisionId>,
    },
    /// A local or module rollback.
    LocalRollback(LocalRollbackRecord),
}

impl DurableReport {
    /// The report id.
    pub fn report_id(&self) -> u64 {
        match self {
            DurableReport::ApplyResult { report_id, .. } => *report_id,
            DurableReport::LocalRollback(record) => record.report_id,
        }
    }

    /// The revision an ApplyResult reports on.
    pub fn apply_revision(&self) -> Option<&RevisionId> {
        match self {
            DurableReport::ApplyResult { revision, .. } => Some(revision),
            DurableReport::LocalRollback(_) => None,
        }
    }

    /// The durable form of a report, if it is one.
    pub fn from_report(report: &Report) -> Option<Self> {
        match report {
            Report::ApplyResult {
                report_id,
                revision,
                outcome,
                applied_revision,
            } => Some(DurableReport::ApplyResult {
                report_id: *report_id,
                revision: revision.clone(),
                outcome: serde_json::to_value(outcome).unwrap_or_default(),
                applied_revision: applied_revision.clone(),
            }),
            Report::LocalRollback(record) => Some(DurableReport::LocalRollback(record.clone())),
            _ => None,
        }
    }

    /// The report to (re-)send, or `None` when a stored outcome no longer parses.
    pub fn to_report(&self) -> Option<Report> {
        match self {
            DurableReport::ApplyResult {
                report_id,
                revision,
                outcome,
                applied_revision,
            } => match serde_json::from_value(outcome.clone()) {
                Ok(outcome) => Some(Report::ApplyResult {
                    report_id: *report_id,
                    revision: revision.clone(),
                    outcome,
                    applied_revision: applied_revision.clone(),
                }),
                Err(err) => {
                    tracing::warn!(report = report_id, error = %err, "dropping a stored apply result that no longer parses");
                    None
                }
            },
            DurableReport::LocalRollback(record) => Some(Report::LocalRollback(record.clone())),
        }
    }
}

/// A local (or module) rollback, persisted until the server acknowledges it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRollbackRecord {
    /// The durable report id (0 in records written by older agents; a fresh
    /// id is assigned when they are loaded).
    #[serde(default)]
    pub report_id: u64,
    /// The revision that was rolled back.
    pub from: RevisionId,
    /// The stable revision applied in its place.
    pub to: RevisionId,
    /// Why the agent decided to roll back.
    pub reason: String,
    /// Modules re-applied, or empty for a whole-revision rollback.
    pub modules: Vec<String>,
    /// The failing checks that justified it.
    pub evidence: Vec<buckle_core::health::HealthResult>,
    /// When the rollback ran.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Whether the rollback apply itself succeeded.
    pub succeeded: bool,
    /// The rollback apply's own error, when it failed.
    #[serde(default)]
    pub rollback_error: Option<String>,
    /// The rollback apply's outcome (`ApplyOutcome` as JSON), when it ran.
    #[serde(default)]
    pub rollback_outcome: Option<serde_json::Value>,
}
