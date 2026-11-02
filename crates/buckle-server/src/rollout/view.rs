//! Converts persisted rollout records into the `buckle-api` DTOs the UI reads.

use super::model::{HostPhase, PhaseRuntime, RolloutHostRecord, RolloutRecord};
use crate::store::ApprovalRecord;
use buckle_api::{
    AuditEntry, PlanGroup, RolloutDetail, RolloutEvent, RolloutHost, RolloutPhase, RolloutSummary,
};
use buckle_core::model::{ApprovalPolicy, Parallelism};
use std::collections::BTreeMap;

pub(crate) fn parallel_str(p: Parallelism) -> String {
    match p {
        Parallelism::Count(n) => n.to_string(),
        Parallelism::Percent(p) => format!("{p}%"),
    }
}

pub(crate) fn approval_str(a: ApprovalPolicy) -> String {
    match a {
        ApprovalPolicy::Auto => "auto",
        ApprovalPolicy::Required => "required",
        ApprovalPolicy::IfDestructive => "if_destructive",
    }
    .into()
}

/// Whether the host's current plan needs an approval it does not have.
pub fn needs_approval(
    phase: Option<&PhaseRuntime>,
    host: &RolloutHostRecord,
    approvals: &[ApprovalRecord],
) -> bool {
    let Some(phase) = phase else { return false };
    let required = match phase.approval {
        ApprovalPolicy::Auto => false,
        ApprovalPolicy::Required => true,
        // A deferred module's plan is only known at apply time, so it may
        // turn out destructive: the plan is reviewed as if it were.
        ApprovalPolicy::IfDestructive => {
            host.runtime.destructive || !host.runtime.deferred_modules.is_empty()
        }
    };
    if !required {
        return false;
    }
    let Some(digest) = &host.plan_digest else {
        return false;
    };
    !approvals
        .iter()
        .any(|a| a.phase == phase.name && a.plan_digests.iter().any(|d| d == digest))
}

/// The host row as the UI sees it.
pub fn host(
    record: &RolloutRecord,
    h: &RolloutHostRecord,
    approvals: &[ApprovalRecord],
) -> RolloutHost {
    let phase = h
        .runtime
        .phase_index
        .and_then(|i| record.runtime.phases.get(i));
    let waiting = matches!(h.state, HostPhase::Planned);
    RolloutHost {
        host: h.host.clone(),
        phase: h.phase.clone(),
        wave: h.wave,
        state: h.state.api(),
        plan_digest: h.plan_digest.clone(),
        plan_changes: h.runtime.plan_changes,
        plan_removals: h.runtime.plan_removals,
        failed_module: h.runtime.failed_module.clone(),
        failed_checks: h.runtime.failed_checks.clone(),
        waived_checks: h.runtime.waived_checks.clone(),
        error: h.runtime.error.clone(),
        updated_at: h.updated_at,
        bake_until: h.runtime.bake_until,
        needs_approval: waiting && needs_approval(phase, h, approvals),
        needs_attention: h.runtime.needs_attention,
        error_detail: crate::views::result_error(h.result.as_ref()),
        run_id: None,
    }
}

/// The rollout summary.
pub fn summary(record: &RolloutRecord, hosts: &[RolloutHostRecord]) -> RolloutSummary {
    let done = hosts
        .iter()
        .filter(|h| matches!(h.state, HostPhase::Healthy | HostPhase::RolledBack))
        .count() as u32;
    let failed = hosts
        .iter()
        .filter(|h| h.state == HostPhase::Failed)
        .count() as u32;
    RolloutSummary {
        id: record.id.clone(),
        revision: record.revision.clone(),
        revision_message: record.runtime.revision_message.clone(),
        state: record.state,
        started_at: record.started_at,
        finished_at: record.finished_at,
        triggered_by: record.triggered_by.clone(),
        hosts_total: hosts.len() as u32,
        hosts_done: done,
        hosts_failed: failed,
        current_phase: record
            .runtime
            .phases
            .get(record.runtime.current_phase)
            .map(|p| p.name.clone()),
        pause_reason: record.runtime.pause.as_ref().map(|p| p.reason.clone()),
        trace_id: record.runtime.trace_id.clone(),
        trace_url: None,
    }
}

/// Hosts grouped by plan digest.
pub fn plan_groups(hosts: &[RolloutHostRecord]) -> Vec<PlanGroup> {
    let mut groups: BTreeMap<String, PlanGroup> = BTreeMap::new();
    for h in hosts {
        let Some(digest) = &h.plan_digest else {
            continue;
        };
        let entry = groups.entry(digest.clone()).or_insert_with(|| PlanGroup {
            digest: digest.clone(),
            hosts: Vec::new(),
            changes: h.runtime.plan_changes.unwrap_or(0),
            removals: h.runtime.plan_removals.unwrap_or(0),
            destructive: h.runtime.destructive,
            plan: crate::views::stored_plan_view(h.plan.as_ref()),
        });
        entry.hosts.push(h.host.clone());
    }
    groups.into_values().collect()
}

/// The full rollout as the detail page shows it. `audit` is the audit trail
/// whose subject is this rollout, newest first.
pub fn detail(
    record: &RolloutRecord,
    hosts: &[RolloutHostRecord],
    approvals: &[ApprovalRecord],
    audit: &[AuditEntry],
) -> RolloutDetail {
    let phases = record
        .runtime
        .phases
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let approval = approvals.iter().rev().find(|a| a.phase == p.name);
            RolloutPhase {
                name: p.name.clone(),
                state: p.state,
                parallel: parallel_str(p.parallel),
                approval: approval_str(p.approval),
                bake_seconds: p.bake_seconds,
                max_failures: parallel_str(p.max_failures),
                virtual_stage: p.virtual_stage,
                hosts: hosts
                    .iter()
                    .filter(|h| h.runtime.phase_index == Some(i))
                    .map(|h| host(record, h, approvals))
                    .collect(),
                approved_by: approval.map(|a| a.approver.clone()),
                approved_at: approval.map(|a| a.at),
                bake_until: p.bake_until,
                health_checks: Vec::new(),
            }
        })
        .collect();
    let events = audit
        .iter()
        .map(|e| RolloutEvent {
            at: e.at,
            kind: e.action.clone(),
            message: e.message.clone(),
            host: e
                .detail
                .as_ref()
                .and_then(|d| d.get("host"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            phase: e
                .detail
                .as_ref()
                .and_then(|d| d.get("phase"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            actor: Some(e.actor.clone()),
        })
        .collect();
    RolloutDetail {
        summary: summary(record, hosts),
        phases,
        plans: plan_groups(hosts),
        events,
    }
}
