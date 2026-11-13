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
    // A target held at its baseline waits for an approval of its failing
    // checks as its starting state, whatever the phase's policy.
    if host.state == HostPhase::Held && host.runtime.held_at_baseline {
        return true;
    }
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
    let intent = host.runtime.approval_digest.as_deref();
    !approvals.iter().any(|a| {
        a.phase == phase.name
            && (a.plan_digests.iter().any(|d| d == digest)
                || intent.is_some_and(|i| a.approval_digests.iter().any(|d| d == i)))
    })
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
    let waiting = matches!(h.state, HostPhase::Planned | HostPhase::Held);
    RolloutHost {
        host: h.host.clone(),
        workload: (!h.workload.is_empty()).then(|| h.workload.clone()),
        phase: h.phase.clone(),
        wave: h.wave,
        state: h.state.api(),
        plan_digest: h.plan_digest.clone(),
        plan_changes: h.runtime.plan_changes,
        plan_removals: h.runtime.plan_removals,
        failed_module: h.runtime.failed_module.clone(),
        failed_checks: h.runtime.failed_checks.clone(),
        waived_checks: h.runtime.waived_checks.clone(),
        baseline_failing: h.runtime.baseline_failing.clone(),
        error: h.runtime.error.clone(),
        updated_at: h.updated_at,
        bake_until: h.runtime.bake_until,
        needs_approval: waiting && needs_approval(phase, h, approvals),
        needs_attention: h.runtime.needs_attention,
        error_detail: h.runtime.error_detail.clone(),
        run_id: None,
        reboot: h
            .runtime
            .reboot
            .as_ref()
            .map(super::model::RebootWait::view),
        kind: h.runtime.kind,
        step: h.runtime.stage,
        executor: h.runtime.executor.clone(),
        bake: h.runtime.bake.clone(),
        preview: h.runtime.preview && h.plan_digest.is_some(),
        drifted_from: h.runtime.drifted_from.clone(),
        removal: h.runtime.removal,
        earlier: h
            .runtime
            .earlier
            .iter()
            .map(|a| buckle_api::RolloutAttempt {
                attempt: a.attempt,
                step: a.stage,
                digest: a.digest.clone(),
                changes: a.changes,
                removals: a.removals,
                state: a.state.api(),
                error: a.error.clone(),
                at: a.at,
                run_id: None,
            })
            .collect(),
        total: (!h.runtime.earlier.is_empty())
            .then(|| rollout_plan(h))
            .flatten()
            .map(|p| buckle_api::ChangeCounts::of_plan(&p)),
    }
}

/// Row `h`'s plan as the rollout's record of it: its current plan with what
/// its earlier attempts and steps of the same revision applied folded in
/// ([`buckle_api::PlanView::absorb`]), so that a retry, which plans only
/// what is left, does not hide what the rollout already changed. The
/// current plan alone stays what approvals and plan groups bind.
pub fn rollout_plan(h: &RolloutHostRecord) -> Option<buckle_api::PlanView> {
    let mut out = crate::views::stored_plan_view(h.plan.as_ref());
    for earlier in h
        .runtime
        .earlier
        .iter()
        .filter_map(|a| crate::views::stored_plan_view(a.plan.as_ref()))
    {
        match &mut out {
            Some(p) if p.revision == earlier.revision => p.absorb(&earlier),
            Some(_) => {}
            None => out = Some(earlier),
        }
    }
    out
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
        name: buckle_api::rollout_name(&record.id),
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
        end_reason: record.runtime.end_reason.clone(),
        superseded_by: record.runtime.superseded_by.clone(),
        trace_id: record.runtime.trace_id.clone(),
        trace_url: None,
        action: record
            .runtime
            .action
            .as_ref()
            .map(|a| buckle_api::RolloutActionInfo {
                module: a.module.clone(),
                action: a.action.clone(),
                label: a.label.clone(),
                selector: a.selector.clone(),
                instances: a.instances,
            }),
        changes: None,
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
        entry.hosts.push(row_key(h));
    }
    groups.into_values().collect()
}

/// How lists of names write a row: the host, or `cluster/workload` for a
/// placement ([`buckle_api::placement_key`]).
pub fn row_key(h: &RolloutHostRecord) -> String {
    buckle_api::placement_key(&h.host, Some(&h.workload))
}

/// Whether a finished row changed nothing: its plan had no changes and no
/// removals, nor had any plan it applied before, so it was counted rather
/// than rolled.
fn unchanged(h: &RolloutHostRecord) -> bool {
    h.runtime.plan_changes == Some(0)
        && h.runtime.plan_removals == Some(0)
        && h.runtime.deferred_modules.is_empty()
        && h.runtime
            .earlier
            .iter()
            .all(|a| a.changes == Some(0) && a.removals == Some(0))
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
                stages: p.stages,
                unchanged: hosts
                    .iter()
                    .filter(|h| h.runtime.phase_index == Some(i) && unchanged(h))
                    .count() as u32,
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
            workload: e
                .detail
                .as_ref()
                .and_then(|d| d.get("workload"))
                .and_then(|v| v.as_str())
                .filter(|w| !w.is_empty())
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
        fleet: None,
        events,
    }
}
