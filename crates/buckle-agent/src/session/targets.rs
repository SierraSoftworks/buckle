//! Work for the cluster targets this host executes for: the session's side
//! of [`crate::cluster`]. Each instruction follows a targeted Assign naming
//! its revision; every report names the target, and the workload placement
//! when the instruction was scoped to one (plans, applies and checks then
//! concern that workload alone).

use super::{Report, Shared, health_error_report};
use crate::cluster::{Assignment, ClusterPlan, TargetSpec};
use crate::sink::ReportSink;
use crate::transport::grpc::set_parent_from_traceparent;
use crate::transport::{Instruction, WireCluster, WireSecret};
use buckle_core::health::{HealthReport, HealthScope};
use buckle_core::plan::RevisionId;
use buckle_core::secrets::InstallKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// How often a bake re-checks a cluster's workloads.
const BAKE_INTERVAL: Duration = Duration::from_secs(10);

impl Shared {
    /// Runs a targeted instruction. Returns the revision an Apply or
    /// Rollback left applied on the cluster, when it succeeded.
    pub(super) async fn target_op(
        &self,
        target: String,
        instruction: Instruction,
    ) -> Option<RevisionId> {
        let workload = instruction.workload().map(str::to_owned);
        match instruction {
            Instruction::Assign {
                revision,
                secrets,
                tree_digest,
                cluster,
                ..
            } => {
                self.target_assign(target, revision, secrets, tree_digest, cluster)
                    .await;
                None
            }
            Instruction::RequestPlan { revision, .. } => {
                let span = tracing::info_span!(
                    "agent.plan",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    buckle.workload = workload.as_deref().unwrap_or(""),
                    buckle.revision = %revision.0,
                );
                self.target_plan(target, workload, revision)
                    .instrument(span)
                    .await;
                None
            }
            Instruction::Apply {
                revision,
                plan_digest,
                approval_digest,
                rollout_id,
                phase,
                traceparent,
                ..
            } => {
                let span = tracing::info_span!(
                    "agent.apply",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    buckle.workload = workload.as_deref().unwrap_or(""),
                    buckle.revision = %revision.0,
                    buckle.rollout = rollout_id.as_deref().unwrap_or(""),
                    buckle.phase = phase.as_deref().unwrap_or(""),
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                let binding = match (approval_digest, plan_digest) {
                    (Some(intent), _) => Binding::Intent(intent),
                    (None, Some(full)) => Binding::Digest(full),
                    (None, None) => Binding::None,
                };
                self.target_apply(target, workload, revision, binding, rollout_id)
                    .instrument(span)
                    .await
            }
            Instruction::Rollback {
                revision,
                reason,
                rollout_id,
                traceparent,
                ..
            } => {
                let span = tracing::info_span!(
                    "agent.apply",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    buckle.revision = %revision.0,
                    buckle.rollout = rollout_id.as_deref().unwrap_or(""),
                    rollback = true
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                tracing::warn!(%target, workload = workload.as_deref().unwrap_or(""), revision = %revision.short(), %reason, "rolling a cluster back on the server's instruction");
                self.target_apply(target, workload, revision, Binding::None, rollout_id)
                    .instrument(span)
                    .await
            }
            Instruction::RunHealth {
                request_id,
                scope,
                modules,
                until,
                traceparent,
                revision,
                ..
            } => {
                let span = tracing::info_span!(
                    "agent.health",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    scope = ?scope,
                    request = %request_id
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                // A placement's checks are its workload's unless the server
                // narrowed them further.
                let modules = match (&workload, modules.is_empty()) {
                    (Some(w), true) => vec![w.clone()],
                    _ => modules,
                };
                self.target_health(
                    target, workload, request_id, scope, modules, until, revision,
                )
                .instrument(span)
                .await;
                None
            }
            Instruction::RunAction {
                action_id,
                module,
                action,
                revision,
                traceparent,
                ..
            } => {
                let span = tracing::info_span!(
                    "agent.action",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    buckle.revision = %revision.0,
                    buckle.module = %module,
                    buckle.action = %action,
                    buckle.action_id = %action_id,
                );
                set_parent_from_traceparent(&span, traceparent.as_deref());
                self.target_action(target, workload, action_id, module, action, revision)
                    .instrument(span)
                    .await;
                None
            }
            other => {
                tracing::debug!(%target, instruction = ?std::mem::discriminant(&other), "nothing to do for a cluster target");
                None
            }
        }
    }

    async fn target_assign(
        &self,
        target: String,
        revision: RevisionId,
        secrets: Vec<WireSecret>,
        tree_digest: Option<String>,
        cluster: Option<WireCluster>,
    ) {
        let assigned = async {
            let cluster = cluster.ok_or_else(|| {
                buckle_core::error::system(
                    format!("The server assigned {target} without describing the cluster."),
                    &["Upgrade the Buckle server to match this agent."],
                )
            })?;
            let spec = target_spec(&cluster)?;
            let rev = self
                .ensure_revision(&revision, tree_digest.as_deref())
                .await?;
            let secrets = Arc::new(self.import_secrets(&secrets));
            let mut data = self.data.lock().await;
            data.assignments.insert(Assignment {
                target: target.clone(),
                spec: Arc::new(spec),
                revision: rev,
                secrets,
            });
            Ok::<_, buckle_core::Error>(())
        }
        .await;
        match assigned {
            Ok(()) => {
                tracing::info!(%target, revision = %revision.short(), "cluster revision assigned")
            }
            Err(err) => {
                tracing::error!(%target, revision = %revision.short(), error = %err.description(), "could not take on the cluster's revision");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: Some(target),
                    })
                    .await;
            }
        }
    }

    async fn assignment(
        &self,
        target: &str,
        revision: &RevisionId,
    ) -> buckle_core::Result<Assignment> {
        self.data
            .lock()
            .await
            .assignments
            .get(target, revision)
            .cloned()
            .ok_or_else(|| {
                buckle_core::error::system(
                    format!(
                        "This agent holds no assignment of {} for {target}.",
                        revision.short()
                    ),
                    &["The server sends one before each request; it re-sends both after a reconnect."],
                )
            })
    }

    async fn target_plan(&self, target: String, workload: Option<String>, revision: RevisionId) {
        let planned = async {
            let a = self.assignment(&target, &revision).await?;
            self.executor.plan(&a, None, workload.as_deref()).await
        }
        .await;
        match planned {
            Ok(planned) => {
                tracing::info!(%target, workload = workload.as_deref().unwrap_or(""), digest = %planned.plan.digest.short(), changes = planned.plan.changes(), "planned the cluster");
                self.sink
                    .send(
                        Report::Plan(planned.plan)
                            .about_placement(Some(&target), workload.as_deref()),
                    )
                    .await;
            }
            Err(err) => {
                tracing::error!(%target, workload = workload.as_deref().unwrap_or(""), error = %err.description(), "planning the cluster failed");
                self.sink
                    .send(
                        Report::PlanFailed {
                            revision,
                            error: err.description(),
                            target: Some(target.clone()),
                        }
                        .about_placement(Some(&target), workload.as_deref()),
                    )
                    .await;
            }
        }
    }

    /// Plans `revision` afresh (the placement of `workload`, or the whole
    /// cluster) and applies it when it still matches what was approved
    /// (`binding`); a rollback binds nothing. Returns the revision when it
    /// is now applied.
    async fn target_apply(
        &self,
        target: String,
        workload: Option<String>,
        revision: RevisionId,
        binding: Binding,
        rollout: Option<String>,
    ) -> Option<RevisionId> {
        let about = |report: Report| report.about_placement(Some(&target), workload.as_deref());
        let planned = async {
            let a = self.assignment(&target, &revision).await?;
            self.executor
                .plan(&a, rollout.as_deref(), workload.as_deref())
                .await
        }
        .await;
        let planned = match planned {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(%target, workload = workload.as_deref().unwrap_or(""), error = %err.description(), "could not plan the cluster to apply");
                self.sink
                    .send(about(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: Some(target.clone()),
                    }))
                    .await;
                return None;
            }
        };
        let fresh = &planned.plan;
        let (approved, matches) = match &binding {
            Binding::Intent(d) => (
                Some(d),
                crate::bundle::digests_match(d, fresh.intent_digest().as_str()),
            ),
            Binding::Digest(d) => (
                Some(d),
                crate::bundle::digests_match(d, fresh.digest.as_str()),
            ),
            Binding::None => (None, true),
        };
        if !matches {
            tracing::warn!(%target, approved = ?approved, fresh = %fresh.digest, "refusing a stale cluster plan");
            self.sink
                .send(about(Report::StalePlan {
                    plan: planned.plan,
                    approved_digest: approved.cloned().unwrap_or_default(),
                }))
                .await;
            return None;
        }
        // Record what really runs when it is not the plan the server holds.
        if !matches!(&binding, Binding::Digest(d) if crate::bundle::digests_match(d, fresh.digest.as_str()))
        {
            self.sink.send(about(Report::Plan(fresh.clone()))).await;
        }
        let outcome = self
            .run_target_apply(&target, workload.as_deref(), &planned)
            .await;
        tracing::info!(%target, workload = workload.as_deref().unwrap_or(""), succeeded = outcome.succeeded, changes = outcome.changes, "cluster apply finished");
        let report_id = self.next_report_id().await;
        let applied = outcome.succeeded.then(|| revision.clone());
        self.send_durable(about(Report::ApplyResult {
            report_id,
            revision,
            outcome,
            applied_revision: applied.clone(),
        }))
        .await;
        applied
    }

    /// Runs an action of a workload on the cluster and reports it (durably).
    async fn target_action(
        &self,
        target: String,
        workload: Option<String>,
        action_id: String,
        module: String,
        action: String,
        revision: RevisionId,
    ) {
        let started = Instant::now();
        let outcome = match self.assignment(&target, &revision).await {
            Err(err) => super::action_failed(&err, started),
            Ok(a) => {
                let (tx, rx) = mpsc::channel(256);
                let forwarder = tokio::spawn(crate::coalesce::forward_for(
                    rx,
                    self.sink.clone(),
                    revision.clone(),
                    String::new(),
                    self.options.event_spacing,
                    crate::coalesce::EventScope {
                        target: Some(target.clone()),
                        workload: workload.clone(),
                        action_id: Some(action_id.clone()),
                    },
                ));
                let outcome = self.executor.run_action(&a, &module, &action, tx).await;
                let _ = forwarder.await;
                outcome
            }
        };
        tracing::info!(%target, succeeded = outcome.succeeded, changes = outcome.changes, "cluster action finished");
        let report_id = self.next_report_id().await;
        self.send_durable(
            Report::ActionResult(crate::transport::ActionRun {
                report_id,
                action_id,
                module,
                action,
                revision,
                outcome,
            })
            .about_placement(Some(&target), workload.as_deref()),
        )
        .await;
    }

    async fn run_target_apply(
        &self,
        target: &str,
        workload: Option<&str>,
        planned: &ClusterPlan,
    ) -> buckle_core::engine::ApplyOutcome {
        let (tx, rx) = mpsc::channel(256);
        let forwarder = tokio::spawn(crate::coalesce::forward_for(
            rx,
            self.sink.clone(),
            planned.plan.revision.clone(),
            planned.plan.digest.to_string(),
            self.options.event_spacing,
            crate::coalesce::EventScope {
                target: Some(target.to_owned()),
                workload: workload.map(str::to_owned),
                action_id: None,
            },
        ));
        let outcome = self.executor.apply(planned, tx).await;
        let _ = forwarder.await;
        outcome
    }

    /// Checks the cluster's workloads (`modules`; the placement's, when the
    /// request was scoped to a `workload`) at `revision`, once or (with
    /// `until`) every [`BAKE_INTERVAL`] until then.
    #[allow(clippy::too_many_arguments)]
    async fn target_health(
        &self,
        target: String,
        workload: Option<String>,
        request_id: String,
        scope: HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        revision: Option<RevisionId>,
    ) {
        let send = |sink: ReportSink, revision: Option<RevisionId>, report: HealthReport| {
            let request_id = request_id.clone();
            let target = target.clone();
            let workload = workload.clone();
            async move {
                sink.send(
                    Report::Health {
                        request_id: Some(request_id),
                        revision,
                        report,
                    }
                    .about_placement(Some(&target), workload.as_deref()),
                )
                .await
            }
        };
        // Nothing applied yet: nothing to check.
        let Some(revision) = revision else {
            send(
                self.sink.clone(),
                None,
                HealthReport {
                    scope,
                    results: Vec::new(),
                },
            )
            .await;
            return;
        };
        let a = match self.assignment(&target, &revision).await {
            Ok(a) => a,
            Err(err) => {
                send(
                    self.sink.clone(),
                    Some(revision),
                    health_error_report(scope, &err),
                )
                .await;
                return;
            }
        };
        let modules = (!modules.is_empty()).then_some(modules);
        let deadline = until.map(|until| {
            Instant::now()
                + (until - chrono::Utc::now())
                    .to_std()
                    .unwrap_or(Duration::ZERO)
        });
        let cancel = self
            .session_cancel
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        let superseded = match (scope, until) {
            (HealthScope::Bake, Some(_)) => {
                self.bakes
                    .token(Some(&target), workload.as_deref(), &request_id)
            }
            _ => CancellationToken::new(),
        };
        loop {
            let report = match self
                .executor
                .health(&a, scope, modules.as_deref(), None)
                .await
            {
                Ok(report) => report,
                Err(err) => health_error_report(scope, &err),
            };
            send(self.sink.clone(), Some(revision.clone()), report).await;
            let Some(deadline) = deadline else { break };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep_until((now + BAKE_INTERVAL).min(deadline)) => {}
                _ = cancel.cancelled() => break,
                _ = superseded.cancelled() => {
                    tracing::info!(%target, request = %request_id, "the server moved past this bake; ending it");
                    break;
                }
            }
        }
    }
}

/// What an apply must match.
enum Binding {
    /// The approved plan's intent digest.
    Intent(String),
    /// The approved plan's full digest.
    Digest(String),
    /// Nothing: a rollback or a force apply.
    None,
}

/// The cluster as the server described it.
pub(super) fn target_spec(cluster: &WireCluster) -> buckle_core::Result<TargetSpec> {
    let kind = cluster.kind.parse().map_err(|e: String| {
        buckle_core::error::system(e, &["Upgrade this agent to match the server."])
    })?;
    let identity = buckle_core::model::ClusterIdentity::parse(&cluster.identity)
        .map_err(|e| buckle_core::error::system(e, &["Upgrade this agent to match the server."]))?;
    let fingerprints = InstallKey::from_slice(&cluster.fingerprint_key).ok_or_else(|| {
        buckle_core::error::system(
            "The server sent no fingerprint key for the cluster.",
            &["Upgrade the Buckle server to match this agent."],
        )
    })?;
    Ok(TargetSpec {
        kind,
        api: (!cluster.api.is_empty()).then(|| cluster.api.clone()),
        identity,
        labels: cluster.labels.clone(),
        fingerprints: Arc::new(fingerprints),
    })
}
