//! Work for the cluster targets this host executes for: the session's side
//! of [`crate::cluster`]. Each instruction follows a targeted Assign naming
//! its revision; every report names the target.

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
use tracing::Instrument;

/// How often a bake re-checks a cluster's workloads.
const BAKE_INTERVAL: Duration = Duration::from_secs(10);

impl Shared {
    pub(super) async fn target_op(&self, target: String, instruction: Instruction) {
        match instruction {
            Instruction::Assign {
                revision,
                secrets,
                tree_digest,
                cluster,
                ..
            } => {
                self.target_assign(target, revision, secrets, tree_digest, cluster)
                    .await
            }
            Instruction::RequestPlan { revision, .. } => {
                let span = tracing::info_span!(
                    "agent.plan",
                    buckle.host = %self.host_id,
                    buckle.target = %target,
                    buckle.revision = %revision.0,
                );
                self.target_plan(target, revision).instrument(span).await
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
                self.target_apply(target, revision, binding, rollout_id)
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
                tracing::warn!(%target, revision = %revision.short(), %reason, "rolling a cluster back on the server's instruction");
                self.target_apply(target, revision, Binding::None, rollout_id)
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
                self.target_health(target, request_id, scope, modules, until, revision)
                    .instrument(span)
                    .await
            }
            other => {
                tracing::debug!(%target, instruction = ?std::mem::discriminant(&other), "nothing to do for a cluster target");
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

    async fn target_plan(&self, target: String, revision: RevisionId) {
        let planned = async {
            let a = self.assignment(&target, &revision).await?;
            self.executor.plan(&a, None).await
        }
        .await;
        match planned {
            Ok(planned) => {
                tracing::info!(%target, digest = %planned.plan.digest.short(), changes = planned.plan.changes(), "planned the cluster");
                self.sink
                    .send(Report::Plan(planned.plan).about(Some(&target)))
                    .await;
            }
            Err(err) => {
                tracing::error!(%target, error = %err.description(), "planning the cluster failed");
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

    /// Plans `revision` afresh and applies it when it still matches what was
    /// approved (`binding`); a rollback binds nothing.
    async fn target_apply(
        &self,
        target: String,
        revision: RevisionId,
        binding: Binding,
        rollout: Option<String>,
    ) {
        let planned = async {
            let a = self.assignment(&target, &revision).await?;
            self.executor.plan(&a, rollout.as_deref()).await
        }
        .await;
        let planned = match planned {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(%target, error = %err.description(), "could not plan the cluster to apply");
                self.sink
                    .send(Report::PlanFailed {
                        revision,
                        error: err.description(),
                        target: Some(target),
                    })
                    .await;
                return;
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
                .send(
                    Report::StalePlan {
                        plan: planned.plan,
                        approved_digest: approved.cloned().unwrap_or_default(),
                    }
                    .about(Some(&target)),
                )
                .await;
            return;
        }
        // Record what really runs when it is not the plan the server holds.
        if !matches!(&binding, Binding::Digest(d) if crate::bundle::digests_match(d, fresh.digest.as_str()))
        {
            self.sink
                .send(Report::Plan(fresh.clone()).about(Some(&target)))
                .await;
        }
        let outcome = self.run_target_apply(&target, &planned).await;
        tracing::info!(%target, succeeded = outcome.succeeded, changes = outcome.changes, "cluster apply finished");
        let report_id = self.next_report_id().await;
        let applied = outcome.succeeded.then(|| revision.clone());
        self.send_durable(
            Report::ApplyResult {
                report_id,
                revision,
                outcome,
                applied_revision: applied,
            }
            .about(Some(&target)),
        )
        .await;
    }

    async fn run_target_apply(
        &self,
        target: &str,
        planned: &ClusterPlan,
    ) -> buckle_core::engine::ApplyOutcome {
        let (tx, rx) = mpsc::channel(256);
        let forwarder = tokio::spawn(crate::coalesce::forward_for(
            rx,
            self.sink.clone(),
            planned.plan.revision.clone(),
            planned.plan.digest.to_string(),
            self.options.event_spacing,
            Some(target.to_owned()),
        ));
        let outcome = self.executor.apply(planned, tx).await;
        let _ = forwarder.await;
        outcome
    }

    /// Checks the cluster's workloads at `revision`, once or (with `until`)
    /// every [`BAKE_INTERVAL`] until then.
    async fn target_health(
        &self,
        target: String,
        request_id: String,
        scope: HealthScope,
        modules: Vec<String>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        revision: Option<RevisionId>,
    ) {
        let send = |sink: ReportSink, revision: Option<RevisionId>, report: HealthReport| {
            let request_id = request_id.clone();
            let target = target.clone();
            async move {
                sink.send(
                    Report::Health {
                        request_id: Some(request_id),
                        revision,
                        report,
                    }
                    .about(Some(&target)),
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
fn target_spec(cluster: &WireCluster) -> buckle_core::Result<TargetSpec> {
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
