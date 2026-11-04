//! Conversions between the transport-neutral agent types and the protobuf
//! messages in [`buckle_proto::v1`].

use super::{
    Hello, HostUpdate, Instruction, LocalRollbackRecord, Report, UpgradeReport, Welcome, WireSecret,
};
use buckle_core::health::{HealthReport, HealthResult, HealthStatus};
use buckle_core::plan::RevisionId;
use buckle_proto::v1 as pb;

/// A protobuf timestamp for a UTC instant.
pub fn timestamp(at: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: at.timestamp(),
        nanos: at.timestamp_subsec_nanos() as i32,
    }
}

/// The UTC instant of a protobuf timestamp, if it is representable.
pub fn from_timestamp(ts: &prost_types::Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn json_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|err| {
        tracing::error!(error = %err, "could not serialise a report document as JSON");
        Vec::new()
    })
}

/// The protobuf Hello.
/// Optional behaviours this agent understands, announced in Hello:
/// `force_plan` (an operator's force apply skips the digest check) and
/// `approval_digest` (an approval binds the plan's intent).
pub const CAPABILITIES: &[&str] = &["force_plan", "approval_digest"];

pub fn hello_to_proto(hello: &Hello) -> pb::Hello {
    pb::Hello {
        host_id: hello.host_id.clone(),
        agent_version: hello.agent_version.clone(),
        labels: hello
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        applied_revision: hello
            .applied_revision
            .as_ref()
            .map(|r| r.0.clone())
            .unwrap_or_default(),
        last_plan_digest: hello.last_plan_digest.clone().unwrap_or_default(),
        token: hello.token.clone().unwrap_or_default(),
        platform: hello.platform.clone(),
        arch: hello.arch.clone(),
        bootstrap: hello.bootstrap,
        unreported_apply_revisions: hello
            .unreported_apply_revisions
            .iter()
            .map(|r| r.0.clone())
            .collect(),
        capabilities: CAPABILITIES.iter().map(|c| (*c).to_owned()).collect(),
        boot_id: hello.boot_id.clone().unwrap_or_default(),
    }
}

fn duration_to_proto(d: std::time::Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

/// The reboot a run asked for, as announced in its ApplyResult.
pub fn reboot_to_proto(reboot: &buckle_core::reboot::RebootRequest) -> pb::Reboot {
    pb::Reboot {
        delay: Some(duration_to_proto(reboot.delay)),
        timeout: Some(duration_to_proto(reboot.timeout)),
        boot_id: reboot.boot_id.clone().unwrap_or_default(),
        requested_by: reboot.requested_by.clone(),
    }
}

/// Secrets from a protobuf bundle.
pub fn secrets_from_proto(bundle: Option<pb::SecretBundle>) -> Vec<WireSecret> {
    bundle
        .map(|b| {
            b.secrets
                .into_iter()
                .map(|mut s| WireSecret {
                    name: std::mem::take(&mut s.name),
                    // Moved, not copied: the wire buffer becomes the
                    // WireSecret's, which zeroes it on drop.
                    value: std::mem::take(&mut s.value),
                    fingerprint: std::mem::take(&mut s.fingerprint),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The instruction a server message carries, or `None` for an empty message.
pub fn instruction_from_proto(message: pb::ServerMessage) -> Option<Instruction> {
    use pb::server_message::Msg;
    Some(match message.msg? {
        Msg::Welcome(w) => Instruction::Welcome(Welcome {
            server_version: w.server_version,
            session_id: w.session_id,
            stable_revision: non_empty(w.stable_revision).map(RevisionId),
            issued_token: non_empty(w.issued_token),
            labels: w.labels.into_iter().collect(),
            status: w.status,
            secrets: secrets_from_proto(w.secrets),
            stable_tree_digest: non_empty(w.stable_tree_digest),
        }),
        Msg::Assign(a) => Instruction::Assign {
            revision: RevisionId(a.revision),
            stable: a.stable,
            secrets: secrets_from_proto(a.secrets),
            tree_digest: non_empty(a.tree_digest),
        },
        Msg::Plan(p) => Instruction::RequestPlan {
            revision: RevisionId(p.revision),
            select: non_empty(p.select),
        },
        // The server always binds a digest: an empty one never matches a plan
        // and is refused as stale, rather than meaning "apply anything".
        // Only an operator's explicit force apply binds nothing.
        Msg::Apply(a) => Instruction::Apply {
            revision: RevisionId(a.revision),
            plan_digest: (!a.force_plan).then_some(a.plan_digest),
            approval_digest: (!a.force_plan)
                .then(|| non_empty(a.approval_digest))
                .flatten(),
            waived_checks: a.waived_checks,
            force: a.force,
            traceparent: non_empty(a.traceparent),
            select: non_empty(a.select),
            rollout_id: non_empty(a.rollout_id),
            phase: non_empty(a.phase),
        },
        Msg::Rollback(r) => Instruction::Rollback {
            revision: RevisionId(r.revision),
            reason: r.reason,
            rollout_id: non_empty(r.rollout_id),
            traceparent: non_empty(r.traceparent),
        },
        Msg::Health(h) => {
            let scope = pb::HealthScope::try_from(h.scope)
                .ok()
                .and_then(pb::HealthScope::to_core)
                .map(scope_to_core)
                .unwrap_or(buckle_core::health::HealthScope::Manual);
            Instruction::RunHealth {
                request_id: h.request_id,
                scope,
                modules: h.modules,
                until: h.until.as_ref().and_then(from_timestamp),
                traceparent: non_empty(h.traceparent),
            }
        }
        Msg::Ping(_) => Instruction::Ping,
        Msg::Upgrade(u) => Instruction::Upgrade {
            version: u.version,
            when_idle: u.when_idle,
        },
        Msg::HostUpdate(u) => Instruction::HostUpdate(HostUpdate {
            status: u.status,
            labels: u.labels.into_iter().collect(),
            stable_revision: non_empty(u.stable_revision).map(RevisionId),
            secrets: secrets_from_proto(u.secrets),
            stable_tree_digest: non_empty(u.stable_tree_digest),
        }),
        Msg::Ack(a) => Instruction::Ack {
            report_ids: a.report_ids,
        },
        Msg::Rejected(r) => Instruction::Rejected {
            reason: r.reason,
            retry: r.retry,
        },
    })
}

/// The core health scope for the protocol mirror type.
pub fn scope_to_core(scope: pb::buckle_health::HealthScope) -> buckle_core::health::HealthScope {
    use buckle_core::health::HealthScope as C;
    use pb::buckle_health::HealthScope as P;
    match scope {
        P::Baseline => C::Baseline,
        P::Bake => C::Bake,
        P::Rollout => C::Rollout,
        P::Drift => C::Drift,
        P::Manual => C::Manual,
    }
}

/// The protocol scope for a core health scope.
pub fn scope_to_proto(scope: buckle_core::health::HealthScope) -> pb::HealthScope {
    use buckle_core::health::HealthScope as C;
    use pb::buckle_health::HealthScope as P;
    let mirror = match scope {
        C::Baseline => P::Baseline,
        C::Bake => P::Bake,
        C::Rollout => P::Rollout,
        C::Drift => P::Drift,
        C::Manual => P::Manual,
    };
    mirror.into()
}

/// A protobuf health result.
pub fn health_result_to_proto(result: &HealthResult) -> pb::HealthResult {
    let (passed, unknown, reason) = match &result.status {
        HealthStatus::Pass => (true, false, String::new()),
        HealthStatus::Fail { reason } => (false, false, reason.clone()),
        HealthStatus::Unknown { reason } => (false, true, reason.clone()),
    };
    pb::HealthResult {
        module: result.module.clone(),
        check: result.check.clone(),
        kind: result.kind.clone(),
        required: result.required,
        passed,
        unknown,
        reason,
        at: Some(timestamp(result.at)),
        duration_ms: result.duration_ms.min(u32::MAX as u64) as u32,
    }
}

/// A protobuf health report.
pub fn health_report_to_proto(
    request_id: Option<&str>,
    revision: Option<&RevisionId>,
    report: &HealthReport,
) -> pb::HealthReport {
    pb::HealthReport {
        scope: scope_to_proto(report.scope) as i32,
        request_id: request_id.unwrap_or_default().to_owned(),
        revision: revision.map(|r| r.0.clone()).unwrap_or_default(),
        results: report.results.iter().map(health_result_to_proto).collect(),
    }
}

/// Whether an unsuccessful outcome is the engine's "needs attention": no
/// module failed or rolled back, but some stayed blocked.
pub fn needs_attention(outcome: &buckle_core::engine::ApplyOutcome) -> bool {
    use buckle_core::engine::ModuleStatus;
    !outcome.succeeded
        && outcome.failed_module.is_none()
        && outcome.rolled_back_modules.is_empty()
        && outcome
            .module_results
            .iter()
            .any(|m| m.status == ModuleStatus::Blocked)
        && !outcome.module_results.iter().any(|m| {
            matches!(
                m.status,
                ModuleStatus::Failed | ModuleStatus::Unhealthy | ModuleStatus::RolledBack
            )
        })
}

/// A structured error on the wire.
pub fn error_detail_to_proto(report: &buckle_core::error::ErrorReport) -> pb::ErrorDetail {
    pb::ErrorDetail {
        kind: match report.kind {
            buckle_core::error::ErrorKind::User => "user",
            buckle_core::error::ErrorKind::System => "system",
        }
        .to_owned(),
        message: report.message.clone(),
        causes: report.causes.clone(),
        advice: report.advice.clone(),
    }
}

/// An apply event on the wire: the event as JSON plus its module, step id,
/// step state and (for a failure) structured error as plain fields.
pub fn apply_event_to_proto(
    revision: &buckle_core::plan::RevisionId,
    plan_digest: &str,
    at: chrono::DateTime<chrono::Utc>,
    event: &buckle_core::engine::ApplyEvent,
) -> pb::ApplyEvent {
    use buckle_core::engine::ApplyEvent as E;
    let module = match event {
        E::ModuleStarted { module }
        | E::ModuleFinished { module, .. }
        | E::StepStarted { module, .. }
        | E::StepFinished { module, .. }
        | E::StepFailed { module, .. }
        | E::StepBlocked { module, .. }
        | E::StepSkipped { module, .. }
        | E::HandlerRun { module, .. }
        | E::HealthChecked { module, .. } => module.clone(),
        E::RemovalRun { .. } | E::FactsRefreshed { .. } => String::new(),
    };
    let step_state = event
        .step_state()
        .and_then(|s| match serde_json::to_value(s) {
            Ok(serde_json::Value::String(s)) => Some(s),
            _ => None,
        })
        .unwrap_or_default();
    pb::ApplyEvent {
        revision: revision.0.clone(),
        plan_digest: plan_digest.to_owned(),
        at: Some(timestamp(at)),
        event_json: json_bytes(event),
        module,
        step_id: event
            .step()
            .map(|(_, id)| id.to_owned())
            .unwrap_or_default(),
        step_state,
        error: match event {
            E::StepFailed {
                error_detail: Some(detail),
                ..
            } => Some(error_detail_to_proto(detail)),
            E::StepFailed { error, .. } => Some(pb::ErrorDetail {
                kind: "system".into(),
                message: error.clone(),
                ..Default::default()
            }),
            _ => None,
        },
    }
}

fn plan_result(plan: &buckle_core::plan::Plan, error: String) -> pb::PlanResult {
    pb::PlanResult {
        module_errors: plan
            .indeterminate
            .iter()
            .filter(|i| i.plan_error)
            .map(|i| format!("{}: {}", i.module, i.reason))
            .collect(),
        revision: plan.revision.0.clone(),
        digest: plan.digest.to_string(),
        plan_json: json_bytes(plan),
        changes: plan.changes() as u32,
        removals: plan.removals.len() as u32,
        destructive: plan.is_destructive(),
        deferred_modules: plan.deferred.iter().map(|d| d.module.clone()).collect(),
        error,
        approval_digest: plan.intent_digest().to_string(),
    }
}

fn local_rollback_to_proto(record: &LocalRollbackRecord) -> pb::LocalRollback {
    pb::LocalRollback {
        from_revision: record.from.0.clone(),
        to_revision: record.to.0.clone(),
        reason: record.reason.clone(),
        at: Some(timestamp(record.at)),
        modules: record.modules.clone(),
        evidence: record.evidence.iter().map(health_result_to_proto).collect(),
        succeeded: record.succeeded,
        report_id: record.report_id,
        rollback_error: record.rollback_error.clone().unwrap_or_default(),
        rollback_outcome_json: record
            .rollback_outcome
            .as_ref()
            .map(json_bytes)
            .unwrap_or_default(),
    }
}

/// The protobuf messages for a report (a drift pass is one message; every
/// other report is exactly one).
pub fn report_to_proto(report: &Report) -> pb::AgentMessage {
    use pb::agent_message::Msg;
    let msg = match report {
        Report::Facts(facts) => Msg::Facts(pb::Facts {
            json: json_bytes(&facts.tree),
            collected_at: Some(timestamp(chrono::Utc::now())),
        }),
        Report::Heartbeat {
            applied_revision,
            busy,
        } => Msg::Heartbeat(pb::Heartbeat {
            at: Some(timestamp(chrono::Utc::now())),
            applied_revision: applied_revision
                .as_ref()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            busy: *busy,
        }),
        Report::Plan(plan) => Msg::Plan(plan_result(plan, String::new())),
        Report::PlanFailed { revision, error } => Msg::Plan(pb::PlanResult {
            revision: revision.0.clone(),
            error: error.clone(),
            ..Default::default()
        }),
        Report::StalePlan {
            plan,
            approved_digest,
        } => Msg::Plan(plan_result(
            plan,
            format!(
                "stale plan: the approved plan was {approved_digest} but the host now plans {}",
                plan.digest
            ),
        )),
        Report::ApplyEvent {
            revision,
            plan_digest,
            at,
            event,
        } => Msg::Apply(apply_event_to_proto(revision, plan_digest, *at, event)),
        Report::ApplyResult {
            report_id,
            revision,
            outcome,
            applied_revision,
        } => Msg::Result(pb::ApplyResult {
            report_id: *report_id,
            needs_attention: needs_attention(outcome),
            revision: revision.0.clone(),
            plan_digest: outcome.plan_digest.to_string(),
            succeeded: outcome.succeeded,
            duration: Some(prost_types::Duration {
                seconds: (outcome.duration_ms / 1000) as i64,
                nanos: ((outcome.duration_ms % 1000) * 1_000_000) as i32,
            }),
            changes: outcome.changes as u32,
            removals: outcome.removals as u32,
            applied_revision: applied_revision
                .as_ref()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            failed_module: outcome.failed_module.clone().unwrap_or_default(),
            rolled_back_modules: outcome.rolled_back_modules.clone(),
            not_started_modules: outcome.not_started.clone(),
            error: outcome.error.clone().unwrap_or_default(),
            error_detail: outcome.error_detail.as_ref().map(error_detail_to_proto),
            outcome_json: json_bytes(outcome),
            stale_modules: outcome
                .module_results
                .iter()
                .filter(|m| m.stale)
                .map(|m| m.module.clone())
                .collect(),
            reboot: outcome.reboot.as_ref().map(reboot_to_proto),
            failed_checks: outcome
                .health
                .as_ref()
                .map(|h| {
                    h.failures(&[])
                        .map(|r| format!("{}/{}", r.module, r.check))
                        .collect()
                })
                .unwrap_or_default(),
        }),
        Report::Health {
            request_id,
            revision,
            report,
        } => Msg::Health(health_report_to_proto(
            request_id.as_deref(),
            revision.as_ref(),
            report,
        )),
        Report::Drift {
            revision,
            plan,
            corrected,
        } => Msg::Drift(pb::Drift {
            revision: revision.0.clone(),
            plan_digest: plan
                .as_ref()
                .map(|p| p.digest.to_string())
                .unwrap_or_default(),
            changes: plan.as_ref().map(|p| p.changes() as u32).unwrap_or(0),
            removals: plan.as_ref().map(|p| p.removals.len() as u32).unwrap_or(0),
            corrected: *corrected,
            plan_json: plan.as_ref().map(json_bytes).unwrap_or_default(),
        }),
        Report::LocalRollback(record) => Msg::Rollback(local_rollback_to_proto(record)),
        Report::Upgrade { version, outcome } => {
            let (outcome, error) = match outcome {
                UpgradeReport::Updated => (pb::UpgradeOutcome::Updated, String::new()),
                UpgradeReport::UpToDate => (pb::UpgradeOutcome::UpToDate, String::new()),
                UpgradeReport::Failed(e) => (pb::UpgradeOutcome::Failed, e.clone()),
            };
            Msg::Upgrade(pb::UpgradeResult {
                version: version.clone(),
                outcome: outcome as i32,
                error,
            })
        }
        Report::RebootFailed { boot_id, error } => Msg::RebootFailed(pb::RebootFailed {
            boot_id: boot_id.clone().unwrap_or_default(),
            error: error.clone(),
        }),
    };
    pb::AgentMessage { msg: Some(msg) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::engine::{ApplyEvent, ApplyOutcome};
    use buckle_core::health::HealthScope;
    use rstest::rstest;

    #[rstest]
    #[case(HealthScope::Baseline)]
    #[case(HealthScope::Bake)]
    #[case(HealthScope::Rollout)]
    #[case(HealthScope::Drift)]
    #[case(HealthScope::Manual)]
    fn health_scopes_survive_the_wire(#[case] scope: HealthScope) {
        let message = pb::ServerMessage {
            msg: Some(pb::server_message::Msg::Health(pb::RunHealth {
                request_id: "r1".into(),
                scope: scope_to_proto(scope) as i32,
                ..Default::default()
            })),
        };
        match instruction_from_proto(message) {
            Some(Instruction::RunHealth {
                scope: got,
                until,
                traceparent,
                ..
            }) => {
                assert_eq!(got, scope);
                assert_eq!(until, None);
                assert_eq!(traceparent, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_empty_apply_digest_stays_bound() {
        let message = pb::ServerMessage {
            msg: Some(pb::server_message::Msg::Apply(pb::Apply {
                revision: "r".into(),
                ..Default::default()
            })),
        };
        match instruction_from_proto(message) {
            Some(Instruction::Apply { plan_digest, .. }) => {
                assert_eq!(plan_digest, Some(String::new()))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn health_results_map_pass_fail_and_unknown() {
        let mk = |status| HealthResult {
            module: "m".into(),
            check: "c".into(),
            kind: "http".into(),
            required: true,
            status,
            at: chrono::Utc::now(),
            duration_ms: 5,
        };
        let pass = health_result_to_proto(&mk(HealthStatus::Pass));
        assert!(pass.passed && !pass.unknown);
        let fail = health_result_to_proto(&mk(HealthStatus::fail("down")));
        assert!(!fail.passed && !fail.unknown);
        assert_eq!(fail.reason, "down");
        let unknown = health_result_to_proto(&mk(HealthStatus::unknown("no curl")));
        assert!(!unknown.passed && unknown.unknown);
    }

    #[test]
    fn blocked_modules_without_failures_need_attention() {
        use buckle_core::engine::{ModuleOutcome, ModuleStatus};
        let plan = crate::testing::plan_with_changes(&RevisionId::new("r"), 1);
        let mut outcome = crate::testing::succeeded(&plan);
        assert!(!needs_attention(&outcome));
        outcome.succeeded = false;
        outcome.module_results = vec![ModuleOutcome {
            duration_ms: 1,
            error: Some("waiting on a lock".into()),
            ..ModuleOutcome::new("m", ModuleStatus::Blocked)
        }];
        let report = Report::ApplyResult {
            report_id: 1,
            revision: RevisionId::new("r"),
            outcome: outcome.clone(),
            applied_revision: None,
        };
        match report_to_proto(&report).msg {
            Some(pb::agent_message::Msg::Result(r)) => {
                assert!(r.needs_attention);
                assert!(!r.succeeded);
            }
            other => panic!("unexpected {other:?}"),
        }
        outcome.failed_module = Some("m".into());
        outcome.module_results[0].status = ModuleStatus::Failed;
        assert!(!needs_attention(&outcome), "a failure is a failure");
    }

    #[rstest]
    #[case::started(
        ApplyEvent::StepStarted { module: "m".into(), task_id: "t".into(), id: "file.copy[/x]".into(), kind: "file.copy".into() },
        "running",
        false
    )]
    #[case::finished(
        ApplyEvent::StepFinished { module: "m".into(), task_id: "t".into(), id: "file.copy[/x]".into(), kind: "file.copy".into(), changed: true, summary: "wrote".into() },
        "changed",
        false
    )]
    #[case::blocked(
        ApplyEvent::StepBlocked { module: "m".into(), task_id: "t".into(), id: "file.copy[/x]".into(), kind: "file.copy".into(), reason: "no tool".into() },
        "blocked",
        false
    )]
    #[case::failed(
        ApplyEvent::StepFailed {
            module: "m".into(), task_id: "t".into(), id: "file.copy[/x]".into(), kind: "file.copy".into(),
            error: "write failed".into(),
            error_detail: Some(buckle_core::error::ErrorReport::user("write failed").with_causes(["disk full"]).with_advice(["free space"])),
        },
        "failed",
        true
    )]
    fn step_events_carry_their_id_state_and_error_on_the_wire(
        #[case] event: ApplyEvent,
        #[case] state: &str,
        #[case] has_error: bool,
    ) {
        let wire = apply_event_to_proto(&RevisionId::new("r"), "d", chrono::Utc::now(), &event);
        assert_eq!(wire.module, "m");
        assert_eq!(wire.step_id, "file.copy[/x]");
        assert_eq!(wire.step_state, state);
        assert_eq!(wire.error.is_some(), has_error);
        if let Some(error) = wire.error {
            assert_eq!(error.kind, "user");
            assert_eq!(error.message, "write failed");
            assert_eq!(error.causes, vec!["disk full"]);
            assert_eq!(error.advice, vec!["free space"]);
        }
        let back: ApplyEvent = serde_json::from_slice(&wire.event_json).unwrap();
        assert_eq!(back.step(), event.step());
    }

    #[test]
    fn the_apply_result_carries_the_whole_outcome_and_its_run_error() {
        let plan = crate::testing::plan_with_changes(&RevisionId::new("r"), 400);
        let mut outcome = crate::testing::succeeded(&plan);
        outcome.succeeded = false;
        outcome.error_detail = Some(
            buckle_core::error::ErrorReport::system("state could not be saved")
                .with_advice(["check the disk"]),
        );
        let report = Report::ApplyResult {
            report_id: 1,
            revision: RevisionId::new("r"),
            outcome: outcome.clone(),
            applied_revision: None,
        };
        match report_to_proto(&report).msg {
            Some(pb::agent_message::Msg::Result(r)) => {
                let back: ApplyOutcome = serde_json::from_slice(&r.outcome_json).unwrap();
                assert_eq!(back.module_results.len(), outcome.module_results.len());
                let detail = r.error_detail.expect("the run error");
                assert_eq!(detail.kind, "system");
                assert_eq!(detail.advice, vec!["check the disk"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn module_plan_errors_travel_with_the_plan() {
        let mut plan = crate::testing::plan_with_changes(&RevisionId::new("r"), 1);
        plan.indeterminate = vec![
            buckle_core::plan::Indeterminate {
                module: "nomad".into(),
                reason: "var 'dc' is undefined".into(),
                plan_error: true,
                error_detail: None,
            },
            buckle_core::plan::Indeterminate {
                module: "docker".into(),
                reason: "facts missing".into(),
                plan_error: false,
                error_detail: None,
            },
        ];
        match report_to_proto(&Report::Plan(plan)).msg {
            Some(pb::agent_message::Msg::Plan(p)) => {
                assert_eq!(p.error, "", "the plan itself is valid");
                assert_eq!(
                    p.module_errors,
                    vec!["nomad: var 'dc' is undefined".to_string()]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wire_secrets_never_debug_their_value() {
        let s = WireSecret {
            name: "db".into(),
            value: b"hunter2".to_vec(),
            fingerprint: "abcd".into(),
        };
        let shown = format!("{s:?}");
        assert!(shown.contains("db"));
        assert!(!shown.contains("hunter2"));
    }

    #[test]
    fn welcome_debug_hides_the_issued_token_and_secret_values() {
        let message = pb::ServerMessage {
            msg: Some(pb::server_message::Msg::Welcome(pb::Welcome {
                issued_token: "per-host-token-value".into(),
                secrets: Some(pb::SecretBundle {
                    secrets: vec![pb::SecretValue {
                        name: "db".into(),
                        value: b"hunter2".to_vec(),
                        fingerprint: "0a1b2c3d".into(),
                    }],
                }),
                ..Default::default()
            })),
        };
        let instruction = instruction_from_proto(message).expect("welcome");
        let shown = format!("{instruction:?}");
        assert!(!shown.contains("per-host-token-value"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(shown.contains("0a1b2c3d"), "{shown}");
    }
}
