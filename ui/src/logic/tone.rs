//! One place that decides what colour a state is and what it is called, so a
//! failed host reads the same on the pipeline, in a table and in a pill.
//!
//! Colours are never chosen here directly: a [`Tone`] names a semantic token
//! (`--ok`, `--danger`, …) and the stylesheet maps it onto the active theme.

use buckle_api::{
    Connection, ErrorKind, HealthSummary, HostReboot, HostRolloutState, HostStatus, HostSummary,
    PhaseState, RevisionStatus, RolloutHost, RolloutState,
};
use chrono::{DateTime, Utc};

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
        HostRolloutState::Rebooting => "rebooting",
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
        HostRolloutState::Applying | HostRolloutState::Rebooting | HostRolloutState::Baking => {
            Tone::Active
        }
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
        HostRolloutState::Rebooting => "block--rebooting",
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
    HostRolloutState::Rebooting,
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
    // A workload (or an old-style cluster row) is round, so it never reads
    // as a host.
    if super::units::is_placement(entry) {
        classes.push("block--cluster");
        label = format!("workload, {label}");
    } else if entry.kind == buckle_api::TargetKind::Cluster {
        classes.push("block--cluster");
        label = format!("cluster, {label}");
    }
    // Nothing to change: solid grey, so the eye lands on what moves. A
    // unit that failed or was held keeps its colour.
    if super::units::unchanged(entry)
        && !entry.needs_attention
        && !matches!(
            entry.state,
            HostRolloutState::Failed | HostRolloutState::Held | HostRolloutState::RolledBack
        )
    {
        classes.push("block--unchanged");
        label.push_str(", unchanged");
    }

    if entry.needs_approval
        && !matches!(
            entry.state,
            HostRolloutState::Healthy | HostRolloutState::Skipped | HostRolloutState::RolledBack
        )
    {
        classes.push("block--needs-approval");
        label.push_str(", needs approval");
    }

    // Its own rollback failed: an operator must look, whatever the state.
    if entry.needs_attention {
        classes.push("block--attention");
        label.push_str(", needs attention");
    }

    if let Some(host) = host {
        let settled = matches!(
            entry.state,
            HostRolloutState::Healthy | HostRolloutState::Baking | HostRolloutState::Applying
        );
        // A placement fails with its own workload, not the whole cluster.
        let failing = match &entry.workload {
            Some(w) => host
                .cluster_status
                .as_ref()
                .is_some_and(|s| s.failing.contains(w)),
            None => host.health == HealthSummary::Failing,
        };
        if settled && failing {
            classes.push("block--regressed");
            label.push_str(", health failing");
        }
        // A rebooting host is offline by design: the state says so already.
        if host.connection == Connection::Offline
            && entry.state != HostRolloutState::Rebooting
            && entry.kind == buckle_api::TargetKind::Host
        {
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

/// How a host outside the rollout on a fleet map looks: coloured by its own
/// health, since the rollout has no state for it.
pub fn outside_block_look(host: Option<&HostSummary>) -> BlockLook {
    let health = host.map(|h| h.health).unwrap_or(HealthSummary::Unknown);
    let class = match health {
        HealthSummary::Healthy => "block--healthy",
        HealthSummary::Failing => "block--failed",
        HealthSummary::Held => "block--held",
        HealthSummary::Unknown => "block--pending",
    };
    let mut classes = vec!["block", class];
    let mut label = format!("not in this rollout, {}", health_label(health));
    if host.is_some_and(|h| h.connection == Connection::Offline) {
        classes.push("block--offline");
        label.push_str(", offline");
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
        RolloutState::Skipped => "skipped",
    }
}

pub fn rollout_state_tone(state: RolloutState) -> Tone {
    match state {
        RolloutState::Detected => Tone::Neutral,
        RolloutState::Planning | RolloutState::Rolling | RolloutState::Baking => Tone::Active,
        RolloutState::AwaitingApproval => Tone::Info,
        RolloutState::Paused => Tone::Warn,
        RolloutState::RollingBack | RolloutState::RolledBack | RolloutState::Skipped => Tone::Mute,
        RolloutState::Completed => Tone::Ok,
        RolloutState::Aborted => Tone::Danger,
    }
}

/// Whether a rollout has stopped for good. Only a live rollout has actions.
pub fn rollout_finished(state: RolloutState) -> bool {
    matches!(
        state,
        RolloutState::Completed
            | RolloutState::RolledBack
            | RolloutState::Aborted
            | RolloutState::Skipped
    )
}

/// Whether a rollout is in progress, as the server's `is_active` has it: a
/// rollout queued behind another (`detected`) is not, so it never takes the
/// running one's place.
pub fn rollout_active(state: RolloutState) -> bool {
    !rollout_finished(state) && state != RolloutState::Detected
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

/// The tag on an error block: who should act on it.
pub fn error_kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::User => "user",
        ErrorKind::System => "system",
    }
}

pub fn error_kind_title(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::User => "Something the configuration author or an operator can fix",
        ErrorKind::System => "Something in Buckle or on the host that needs investigating",
    }
}

pub fn error_kind_class(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::User => "error-block--user",
        ErrorKind::System => "error-block--system",
    }
}

/// How loudly a rollout host's `error` is shown. The field carries notes as
/// well as failures (why a host was skipped, held or has no plan), and a
/// skip reason drawn in red would read as a failure.
pub fn host_error_tone(entry: &RolloutHost) -> Tone {
    if entry.needs_attention {
        return Tone::Danger;
    }
    match entry.state {
        HostRolloutState::Failed => Tone::Danger,
        HostRolloutState::Held => Tone::Warn,
        HostRolloutState::RolledBack => Tone::Mute,
        _ => Tone::Neutral,
    }
}

/// A rollout host's error in one line, for a tooltip: the structured
/// headline when there is one, the reported string otherwise.
pub fn host_error_headline(entry: &RolloutHost) -> Option<String> {
    let said = |e: &&str| !e.trim().is_empty();
    entry
        .error_detail
        .as_ref()
        .map(|e| e.message.as_str())
        .filter(said)
        .or_else(|| entry.error.as_deref().filter(said))
        .map(str::to_string)
}

/// The reboot a rollout host is still being waited for: rebooting, not yet
/// back, and not yet past its deadline.
pub fn awaited_reboot(entry: &RolloutHost, now: DateTime<Utc>) -> Option<&HostReboot> {
    entry.reboot.as_ref().filter(|r| {
        entry.state == HostRolloutState::Rebooting && r.back_at.is_none() && r.until > now
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            reboot: None,
            kind: Default::default(),
            step: 0,
            workload: None,
            executor: None,
            bake: None,
            preview: false,
            drifted_from: None,
            needs_approval: false,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        }
    }

    fn host(health: HealthSummary, connection: Connection) -> HostSummary {
        HostSummary {
            id: "pi-1".into(),
            kind: Default::default(),
            cluster: None,
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
            upgrade: None,
            cluster_status: None,
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
        assert_eq!(
            rollout_state_tone(RolloutState::Skipped),
            Tone::Mute,
            "a superseded rollout did not fail"
        );
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

        // Offline is what a reboot looks like, not news.
        let rebooting = block_look(
            &entry(HostRolloutState::Rebooting),
            Some(&host(HealthSummary::Healthy, Connection::Offline)),
        );
        assert_eq!(rebooting.classes, vec!["block", "block--rebooting"]);
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
    fn a_host_whose_own_rollback_failed_needs_attention() {
        let mut stuck = entry(HostRolloutState::Failed);
        stuck.needs_attention = true;
        let look = block_look(&stuck, None);
        assert!(look.classes.contains(&"block--attention"));
        assert_eq!(look.label, "failed, needs attention");
        assert!(
            !block_look(&entry(HostRolloutState::Failed), None)
                .classes
                .contains(&"block--attention")
        );
    }

    #[test]
    fn a_host_error_is_only_red_when_something_failed() {
        let mut skipped = entry(HostRolloutState::Skipped);
        skipped.error = Some("Skipped by bob: board swap.".into());
        assert_eq!(host_error_tone(&skipped), Tone::Neutral);
        assert_eq!(host_error_tone(&entry(HostRolloutState::Held)), Tone::Warn);
        assert_eq!(
            host_error_tone(&entry(HostRolloutState::Failed)),
            Tone::Danger
        );
        let mut stuck = entry(HostRolloutState::RolledBack);
        stuck.needs_attention = true;
        assert_eq!(host_error_tone(&stuck), Tone::Danger);
    }

    #[test]
    fn a_tooltip_prefers_the_structured_headline() {
        let mut e = entry(HostRolloutState::Failed);
        assert_eq!(host_error_headline(&e), None);
        e.error = Some("a: b: c".into());
        assert_eq!(host_error_headline(&e).as_deref(), Some("a: b: c"));
        e.error_detail = Some(buckle_api::ErrorView::user("nomad failed"));
        assert_eq!(host_error_headline(&e).as_deref(), Some("nomad failed"));
        // A structured error with nothing to say does not hide the string.
        e.error_detail = Some(buckle_api::ErrorView::user("  "));
        assert_eq!(host_error_headline(&e).as_deref(), Some("a: b: c"));
        e.error = Some(String::new());
        assert_eq!(host_error_headline(&e), None);
        assert_eq!(error_kind_label(ErrorKind::User), "user");
        assert_eq!(error_kind_label(ErrorKind::System), "system");
        assert_ne!(
            error_kind_class(ErrorKind::User),
            error_kind_class(ErrorKind::System)
        );
    }

    #[test]
    fn only_terminal_rollouts_count_as_finished() {
        assert!(rollout_finished(RolloutState::Completed));
        assert!(rollout_finished(RolloutState::Aborted));
        assert!(rollout_finished(RolloutState::RolledBack));
        assert!(rollout_finished(RolloutState::Skipped));
        assert!(!rollout_finished(RolloutState::Paused));
        assert!(!rollout_finished(RolloutState::RollingBack));
    }
}
