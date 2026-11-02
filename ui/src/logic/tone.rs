//! One place that decides what colour a state is and what it is called, so a
//! failed host reads the same on the pipeline, in a table and in a pill.
//!
//! Colours are never chosen here directly: a [`Tone`] names a semantic token
//! (`--ok`, `--danger`, …) and the stylesheet maps it onto the active theme.

use buckle_api::{
    Connection, HealthSummary, HostRolloutState, HostStatus, HostSummary, PhaseState,
    RevisionStatus, RolloutHost, RolloutState,
};

/// How much attention something asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Danger,
    /// In motion: applying, planning, rolling.
    Active,
    /// Informational: planned, ready.
    Info,
    /// Undone on purpose: rolled back.
    Mute,
    /// Neither good nor bad: pending, skipped, unknown.
    Neutral,
}

impl Tone {
    /// The modifier class a pill (or anything else toned) takes.
    pub fn class(self) -> &'static str {
        match self {
            Tone::Ok => "tone-ok",
            Tone::Warn => "tone-warn",
            Tone::Danger => "tone-danger",
            Tone::Active => "tone-active",
            Tone::Info => "tone-info",
            Tone::Mute => "tone-mute",
            Tone::Neutral => "tone-neutral",
        }
    }
}

pub fn host_state_label(state: HostRolloutState) -> &'static str {
    match state {
        HostRolloutState::Pending => "pending",
        HostRolloutState::PlanPending => "plan pending",
        HostRolloutState::Planned => "planned",
        HostRolloutState::Held => "held",
        HostRolloutState::Applying => "applying",
        HostRolloutState::Baking => "baking",
        HostRolloutState::Healthy => "healthy",
        HostRolloutState::Failed => "failed",
        HostRolloutState::RolledBack => "rolled back",
        HostRolloutState::Skipped => "skipped",
    }
}

pub fn host_state_tone(state: HostRolloutState) -> Tone {
    match state {
        HostRolloutState::Pending | HostRolloutState::PlanPending | HostRolloutState::Skipped => {
            Tone::Neutral
        }
        HostRolloutState::Planned => Tone::Info,
        HostRolloutState::Held => Tone::Warn,
        HostRolloutState::Applying | HostRolloutState::Baking => Tone::Active,
        HostRolloutState::Healthy => Tone::Ok,
        HostRolloutState::Failed => Tone::Danger,
        HostRolloutState::RolledBack => Tone::Mute,
    }
}

/// The block modifier for a rollout state (`block--baking`). Kept apart from
/// the tone because two states share a tone but not a look: baking is the
/// active colour with a slow pulse, applying the same colour solid.
pub fn host_state_class(state: HostRolloutState) -> &'static str {
    match state {
        HostRolloutState::Pending => "block--pending",
        HostRolloutState::PlanPending => "block--plan-pending",
        HostRolloutState::Planned => "block--planned",
        HostRolloutState::Held => "block--held",
        HostRolloutState::Applying => "block--applying",
        HostRolloutState::Baking => "block--baking",
        HostRolloutState::Healthy => "block--healthy",
        HostRolloutState::Failed => "block--failed",
        HostRolloutState::RolledBack => "block--rolled-back",
        HostRolloutState::Skipped => "block--skipped",
    }
}

/// Every state in the order the legend lists them.
pub const LEGEND: &[HostRolloutState] = &[
    HostRolloutState::Pending,
    HostRolloutState::PlanPending,
    HostRolloutState::Planned,
    HostRolloutState::Held,
    HostRolloutState::Applying,
    HostRolloutState::Baking,
    HostRolloutState::Healthy,
    HostRolloutState::Failed,
    HostRolloutState::RolledBack,
    HostRolloutState::Skipped,
];

/// What a host block looks like: its rollout state, plus what the fleet's
/// latest reading says about the host itself. The two disagree on purpose
/// when it matters: a host the rollout calls healthy that the last drift
/// check found failing has regressed, and the block says so with a ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLook {
    pub classes: Vec<&'static str>,
    /// The one-line state for a tooltip and a screen reader.
    pub label: String,
}

pub fn block_look(entry: &RolloutHost, host: Option<&HostSummary>) -> BlockLook {
    let mut classes = vec!["block", host_state_class(entry.state)];
    let mut label = host_state_label(entry.state).to_string();

    if entry.needs_approval
        && !matches!(
            entry.state,
            HostRolloutState::Healthy | HostRolloutState::Skipped | HostRolloutState::RolledBack
        )
    {
        classes.push("block--needs-approval");
        label.push_str(", needs approval");
    }

    if let Some(host) = host {
        let settled = matches!(
            entry.state,
            HostRolloutState::Healthy | HostRolloutState::Baking | HostRolloutState::Applying
        );
        if settled && host.health == HealthSummary::Failing {
            classes.push("block--regressed");
            label.push_str(", health failing");
        }
        if host.connection == Connection::Offline {
            classes.push("block--offline");
            label.push_str(", offline");
        }
    }

    if !entry.waived_checks.is_empty() {
        classes.push("block--waived");
        label.push_str(", checks waived");
    }

    BlockLook { classes, label }
}

pub fn rollout_state_label(state: RolloutState) -> &'static str {
    match state {
        RolloutState::Detected => "detected",
        RolloutState::Planning => "planning",
        RolloutState::AwaitingApproval => "awaiting approval",
        RolloutState::Rolling => "rolling",
        RolloutState::Baking => "baking",
        RolloutState::Paused => "paused",
        RolloutState::RollingBack => "rolling back",
        RolloutState::RolledBack => "rolled back",
        RolloutState::Completed => "completed",
        RolloutState::Aborted => "aborted",
    }
}

pub fn rollout_state_tone(state: RolloutState) -> Tone {
    match state {
        RolloutState::Detected => Tone::Neutral,
        RolloutState::Planning | RolloutState::Rolling | RolloutState::Baking => Tone::Active,
        RolloutState::AwaitingApproval => Tone::Info,
        RolloutState::Paused => Tone::Warn,
        RolloutState::RollingBack | RolloutState::RolledBack => Tone::Mute,
        RolloutState::Completed => Tone::Ok,
        RolloutState::Aborted => Tone::Danger,
    }
}

/// Whether a rollout has stopped for good. Only a live rollout has actions.
pub fn rollout_finished(state: RolloutState) -> bool {
    matches!(
        state,
        RolloutState::Completed | RolloutState::RolledBack | RolloutState::Aborted
    )
}

pub fn phase_state_label(state: PhaseState) -> &'static str {
    match state {
        PhaseState::Pending => "pending",
        PhaseState::AwaitingApproval => "awaiting approval",
        PhaseState::Running => "running",
        PhaseState::Baking => "baking",
        PhaseState::Done => "done",
        PhaseState::Paused => "paused",
        PhaseState::Skipped => "skipped",
        PhaseState::Open => "open",
    }
}

pub fn phase_state_tone(state: PhaseState) -> Tone {
    match state {
        PhaseState::Pending | PhaseState::Skipped => Tone::Neutral,
        PhaseState::AwaitingApproval => Tone::Info,
        PhaseState::Running | PhaseState::Baking | PhaseState::Open => Tone::Active,
        PhaseState::Done => Tone::Ok,
        PhaseState::Paused => Tone::Warn,
    }
}

pub fn health_label(health: HealthSummary) -> &'static str {
    match health {
        HealthSummary::Healthy => "healthy",
        HealthSummary::Failing => "failing",
        HealthSummary::Held => "held",
        HealthSummary::Unknown => "unknown",
    }
}

pub fn health_tone(health: HealthSummary) -> Tone {
    match health {
        HealthSummary::Healthy => Tone::Ok,
        HealthSummary::Failing => Tone::Danger,
        HealthSummary::Held => Tone::Warn,
        HealthSummary::Unknown => Tone::Neutral,
    }
}

pub fn host_status_label(status: HostStatus) -> &'static str {
    match status {
        HostStatus::Pending => "pending approval",
        HostStatus::Active => "active",
        HostStatus::Paused => "paused",
    }
}

pub fn host_status_tone(status: HostStatus) -> Tone {
    match status {
        HostStatus::Pending => Tone::Info,
        HostStatus::Active => Tone::Ok,
        HostStatus::Paused => Tone::Warn,
    }
}

pub fn revision_status_tone(status: RevisionStatus) -> Tone {
    match status {
        RevisionStatus::Valid => Tone::Ok,
        RevisionStatus::Rejected => Tone::Danger,
    }
}

/// A health check result (`pass`, `fail`, `unknown`, `flap`). The wire value
/// is a string so the server can add kinds; anything unrecognised is neutral.
pub fn check_status_tone(status: &str) -> Tone {
    match status.to_ascii_lowercase().as_str() {
        "pass" | "passed" | "ok" | "healthy" => Tone::Ok,
        "fail" | "failed" | "failing" | "error" => Tone::Danger,
        "flap" | "flapping" | "warn" | "warning" => Tone::Warn,
        "waived" => Tone::Mute,
        _ => Tone::Neutral,
    }
}

/// A run or audit outcome (`success`, `failed`, `rolled_back`, `denied`, …).
pub fn outcome_tone(outcome: &str) -> Tone {
    let outcome = outcome.to_ascii_lowercase();
    if outcome.contains("roll") {
        Tone::Mute
    } else if outcome.contains("fail") || outcome.contains("error") {
        Tone::Danger
    } else if outcome.contains("denied") || outcome.contains("refused") {
        Tone::Warn
    } else if matches!(
        outcome.as_str(),
        "success" | "succeeded" | "ok" | "applied" | "no_change" | "noop"
    ) {
        Tone::Ok
    } else {
        Tone::Neutral
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn entry(state: HostRolloutState) -> RolloutHost {
        RolloutHost {
            host: "pi-1".into(),
            phase: "canary".into(),
            wave: Some(1),
            state,
            plan_digest: None,
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            needs_approval: false,
        }
    }

    fn host(health: HealthSummary, connection: Connection) -> HostSummary {
        HostSummary {
            id: "pi-1".into(),
            status: HostStatus::Active,
            connection,
            agent_labels: BTreeMap::new(),
            labels: BTreeMap::new(),
            effective_labels: BTreeMap::new(),
            groups: vec![],
            platform: None,
            arch: None,
            agent_version: None,
            applied_revision: None,
            health,
            health_at: None,
            drifted: false,
            first_seen: Utc::now(),
            last_seen: None,
            notes: String::new(),
        }
    }

    #[test]
    fn every_rollout_state_has_its_own_block_class_and_a_label() {
        let mut classes: Vec<_> = LEGEND.iter().map(|s| host_state_class(*s)).collect();
        classes.sort();
        classes.dedup();
        assert_eq!(
            classes.len(),
            LEGEND.len(),
            "two states would look the same"
        );
        for state in LEGEND {
            assert!(!host_state_label(*state).is_empty());
        }
    }

    #[test]
    fn failure_is_danger_and_success_is_ok_everywhere() {
        assert_eq!(host_state_tone(HostRolloutState::Failed), Tone::Danger);
        assert_eq!(host_state_tone(HostRolloutState::Healthy), Tone::Ok);
        assert_eq!(host_state_tone(HostRolloutState::Held), Tone::Warn);
        assert_eq!(rollout_state_tone(RolloutState::Aborted), Tone::Danger);
        assert_eq!(rollout_state_tone(RolloutState::Completed), Tone::Ok);
        assert_eq!(rollout_state_tone(RolloutState::Paused), Tone::Warn);
        assert_eq!(health_tone(HealthSummary::Failing), Tone::Danger);
        assert_eq!(check_status_tone("FAIL"), Tone::Danger);
        assert_eq!(check_status_tone("pass"), Tone::Ok);
        assert_eq!(check_status_tone("something-new"), Tone::Neutral);
        assert_eq!(outcome_tone("success"), Tone::Ok);
        assert_eq!(outcome_tone("apply_failed"), Tone::Danger);
        assert_eq!(outcome_tone("rolled_back"), Tone::Mute);
        assert_eq!(outcome_tone("denied"), Tone::Warn);
    }

    #[test]
    fn a_block_shows_the_hosts_own_health_when_it_contradicts_the_rollout() {
        let healthy = entry(HostRolloutState::Healthy);
        let plain = block_look(
            &healthy,
            Some(&host(HealthSummary::Healthy, Connection::Online)),
        );
        assert_eq!(plain.classes, vec!["block", "block--healthy"]);
        assert_eq!(plain.label, "healthy");

        let regressed = block_look(
            &healthy,
            Some(&host(HealthSummary::Failing, Connection::Offline)),
        );
        assert!(regressed.classes.contains(&"block--regressed"));
        assert!(regressed.classes.contains(&"block--offline"));
        assert_eq!(regressed.label, "healthy, health failing, offline");

        // A host that has not been touched yet is not "regressed" by a failing
        // reading: that is what the baseline hold is for.
        let pending = block_look(
            &entry(HostRolloutState::Pending),
            Some(&host(HealthSummary::Failing, Connection::Online)),
        );
        assert!(!pending.classes.contains(&"block--regressed"));
    }

    #[test]
    fn a_stale_plan_is_marked_until_the_host_is_finished() {
        let mut planned = entry(HostRolloutState::Planned);
        planned.needs_approval = true;
        assert!(
            block_look(&planned, None)
                .classes
                .contains(&"block--needs-approval")
        );
        let mut done = entry(HostRolloutState::Healthy);
        done.needs_approval = true;
        assert!(
            !block_look(&done, None)
                .classes
                .contains(&"block--needs-approval")
        );
        let mut waived = entry(HostRolloutState::Baking);
        waived.waived_checks = vec!["nomad/api".into()];
        assert_eq!(block_look(&waived, None).label, "baking, checks waived");
    }

    #[test]
    fn only_terminal_rollouts_count_as_finished() {
        assert!(rollout_finished(RolloutState::Completed));
        assert!(rollout_finished(RolloutState::Aborted));
        assert!(rollout_finished(RolloutState::RolledBack));
        assert!(!rollout_finished(RolloutState::Paused));
        assert!(!rollout_finished(RolloutState::RollingBack));
    }
}
