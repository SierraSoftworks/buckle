//! What a run report says, decided once: the colour, glyph and name of every
//! run, module and step state, which modules open by default, how long
//! things took, and what a host is doing right now.

use buckle_api::{ModuleReport, ModuleState, RunReport, RunState, StepReport, StepState};
use chrono::{DateTime, Utc};

use super::tone::Tone;

pub fn run_label(state: RunState) -> &'static str {
    match state {
        RunState::Pending => "pending",
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::NeedsAttention => "needs attention",
        RunState::RolledBack => "rolled back",
        RunState::StalePlan => "stale plan",
    }
}

pub fn run_tone(state: RunState) -> Tone {
    match state {
        RunState::Pending => Tone::Neutral,
        RunState::Running => Tone::Active,
        RunState::Succeeded => Tone::Ok,
        RunState::Failed => Tone::Danger,
        RunState::NeedsAttention | RunState::StalePlan => Tone::Warn,
        RunState::RolledBack => Tone::Mute,
    }
}

/// Whether a run has ended, one way or another.
pub fn run_finished(state: RunState) -> bool {
    !matches!(state, RunState::Pending | RunState::Running)
}

pub fn module_label(state: ModuleState) -> &'static str {
    match state {
        ModuleState::Pending => "pending",
        ModuleState::Running => "running",
        ModuleState::Applied => "applied",
        ModuleState::Unchanged => "unchanged",
        ModuleState::Failed => "failed",
        ModuleState::Unhealthy => "unhealthy",
        ModuleState::RolledBack => "rolled back",
        ModuleState::NotStarted => "not started",
        ModuleState::Deferred => "deferred",
        ModuleState::Indeterminate => "indeterminate",
        ModuleState::Blocked => "blocked",
        ModuleState::Skipped => "skipped",
    }
}

pub fn module_tone(state: ModuleState) -> Tone {
    match state {
        ModuleState::Pending
        | ModuleState::Unchanged
        | ModuleState::NotStarted
        | ModuleState::Skipped => Tone::Neutral,
        ModuleState::Running => Tone::Active,
        ModuleState::Applied => Tone::Ok,
        ModuleState::Failed | ModuleState::Unhealthy => Tone::Danger,
        ModuleState::RolledBack => Tone::Mute,
        ModuleState::Deferred => Tone::Info,
        ModuleState::Indeterminate | ModuleState::Blocked => Tone::Warn,
    }
}

/// The BEM modifier and `data-state` of a module (`run-module--rolled-back`).
pub fn module_slug(state: ModuleState) -> &'static str {
    match state {
        ModuleState::Pending => "pending",
        ModuleState::Running => "running",
        ModuleState::Applied => "applied",
        ModuleState::Unchanged => "unchanged",
        ModuleState::Failed => "failed",
        ModuleState::Unhealthy => "unhealthy",
        ModuleState::RolledBack => "rolled-back",
        ModuleState::NotStarted => "not-started",
        ModuleState::Deferred => "deferred",
        ModuleState::Indeterminate => "indeterminate",
        ModuleState::Blocked => "blocked",
        ModuleState::Skipped => "skipped",
    }
}

pub fn step_label(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Changed => "changed",
        StepState::Unchanged => "unchanged",
        StepState::Failed => "failed",
        StepState::Blocked => "blocked",
        StepState::Skipped => "skipped",
        StepState::NotRun => "not run",
        StepState::RolledBack => "rolled back",
    }
}

/// Pending grey, running the accent, changed green, unchanged muted, failed
/// red, blocked amber, skipped and not run dim, rolled back purple.
pub fn step_tone(state: StepState) -> Tone {
    match state {
        StepState::Pending | StepState::Unchanged | StepState::Skipped | StepState::NotRun => {
            Tone::Neutral
        }
        StepState::Running => Tone::Active,
        StepState::Changed => Tone::Ok,
        StepState::Failed => Tone::Danger,
        StepState::Blocked => Tone::Warn,
        StepState::RolledBack => Tone::Mute,
    }
}

/// A glyph per state, so the states differ by shape as well as colour.
pub fn step_glyph(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "○",
        StepState::Running => "◉",
        StepState::Changed => "✓",
        StepState::Unchanged => "=",
        StepState::Failed => "✕",
        StepState::Blocked => "!",
        StepState::Skipped => "↷",
        StepState::NotRun => "–",
        StepState::RolledBack => "↶",
    }
}

/// The BEM modifier and `data-state` of a step (`run-step--not-run`).
pub fn step_slug(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Changed => "changed",
        StepState::Unchanged => "unchanged",
        StepState::Failed => "failed",
        StepState::Blocked => "blocked",
        StepState::Skipped => "skipped",
        StepState::NotRun => "not-run",
        StepState::RolledBack => "rolled-back",
    }
}

/// Every step state, in the order a legend would list them.
#[cfg(test)]
pub const STEP_STATES: &[StepState] = &[
    StepState::Pending,
    StepState::Running,
    StepState::Changed,
    StepState::Unchanged,
    StepState::Failed,
    StepState::Blocked,
    StepState::Skipped,
    StepState::NotRun,
    StepState::RolledBack,
];

/// A module state an operator has to read: it failed, its health failed, it
/// was rolled back, steps stayed blocked, or it could not be decided.
pub fn module_problem(state: ModuleState) -> bool {
    matches!(
        state,
        ModuleState::Failed
            | ModuleState::Unhealthy
            | ModuleState::RolledBack
            | ModuleState::Blocked
            | ModuleState::Indeterminate
    )
}

/// Whether a module needs reading: a problem state, an error of its own, or
/// a failed or blocked step (which a live update can report before the
/// module's own state follows).
pub fn module_has_problem(m: &ModuleReport) -> bool {
    module_problem(m.state)
        || m.error.is_some()
        || m.steps
            .iter()
            .chain(&m.handlers)
            .any(|s| matches!(s.state, StepState::Failed | StepState::Blocked))
}

/// Problems open by default, and so does the module that is running, since
/// it is the one being watched; everything else starts collapsed.
pub fn expanded_by_default(m: &ModuleReport) -> bool {
    module_has_problem(m) || m.state == ModuleState::Running
}

/// The step a module is on: the first running task, else a running handler.
pub fn current_step(m: &ModuleReport) -> Option<&StepReport> {
    m.steps
        .iter()
        .chain(&m.handlers)
        .find(|s| s.state == StepState::Running)
}

/// The step's key without its kind, since the kind is shown beside it:
/// `file.template[/etc/x]` → `/etc/x`. An id of another shape is kept whole.
pub fn step_key<'a>(kind: &str, id: &'a str) -> &'a str {
    id.strip_prefix(kind)
        .and_then(|rest| rest.strip_prefix('['))
        .and_then(|rest| rest.strip_suffix(']'))
        .filter(|key| !key.is_empty())
        .unwrap_or(id)
}

/// How long something took, or has been running so far: the reported
/// duration, else the span between its timestamps, else (while running) the
/// time since it started.
pub fn elapsed_ms(
    duration_ms: Option<u64>,
    started: Option<DateTime<Utc>>,
    finished: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    running: bool,
) -> Option<u64> {
    if duration_ms.is_some() {
        return duration_ms;
    }
    let started = started?;
    let end = match finished {
        Some(f) => f,
        None if running => now,
        None => return None,
    };
    Some(end.signed_duration_since(started).num_milliseconds().max(0) as u64)
}

/// Steps finished out of all of them (tasks, handlers and removals), for a
/// progress bar.
pub fn step_progress(r: &RunReport) -> (u32, u32) {
    let s = &r.counts.steps;
    let total = s.pending
        + s.running
        + s.changed
        + s.unchanged
        + s.failed
        + s.blocked
        + s.skipped
        + s.not_run
        + s.rolled_back;
    (total - s.pending - s.running, total)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use buckle_api::{ErrorView, RunCounts};

    pub fn step(id: &str, state: StepState) -> StepReport {
        StepReport {
            id: format!("file.copy[{id}]"),
            kind: "file.copy".into(),
            name: None,
            summary: format!("copy {id}"),
            state,
            result: None,
            diff: None,
            destructive: false,
            handler: false,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            error: None,
        }
    }

    pub fn module(name: &str, state: ModuleState, steps: Vec<StepReport>) -> ModuleReport {
        ModuleReport {
            name: name.into(),
            state,
            note: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            changes: 0,
            error: None,
            steps,
            handlers: vec![],
            health: vec![],
        }
    }

    pub fn report(modules: Vec<ModuleReport>) -> RunReport {
        let mut r = RunReport {
            id: "run-1".into(),
            host: "pi-1".into(),
            revision: "abc".into(),
            plan_digest: "d1".into(),
            rollout_id: Some("42".into()),
            state: RunState::Running,
            started_at: None,
            finished_at: None,
            updated_at: Utc::now(),
            counts: RunCounts::default(),
            modules,
            removals: vec![],
            error: None,
            trace_id: None,
            trace_url: None,
        };
        r.recount();
        r
    }

    #[test]
    fn every_step_state_has_its_own_glyph_slug_and_label() {
        for list in [
            STEP_STATES
                .iter()
                .map(|s| step_glyph(*s))
                .collect::<Vec<_>>(),
            STEP_STATES.iter().map(|s| step_slug(*s)).collect(),
            STEP_STATES.iter().map(|s| step_label(*s)).collect(),
        ] {
            let mut unique = list.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                STEP_STATES.len(),
                "two states look alike: {list:?}"
            );
        }
    }

    #[test]
    fn step_states_take_the_colours_a_reader_expects() {
        assert_eq!(step_tone(StepState::Pending), Tone::Neutral);
        assert_eq!(step_tone(StepState::Running), Tone::Active);
        assert_eq!(step_tone(StepState::Changed), Tone::Ok);
        assert_eq!(step_tone(StepState::Unchanged), Tone::Neutral);
        assert_eq!(step_tone(StepState::Failed), Tone::Danger);
        assert_eq!(step_tone(StepState::Blocked), Tone::Warn);
        assert_eq!(step_tone(StepState::Skipped), Tone::Neutral);
        assert_eq!(step_tone(StepState::NotRun), Tone::Neutral);
        assert_eq!(step_tone(StepState::RolledBack), Tone::Mute);
        assert_eq!(step_glyph(StepState::Failed), "✕");
        assert_eq!(step_slug(StepState::NotRun), "not-run");
        assert_eq!(step_slug(StepState::RolledBack), "rolled-back");
    }

    #[test]
    fn module_and_run_states_agree_with_the_step_colours() {
        assert_eq!(
            module_tone(ModuleState::Applied),
            step_tone(StepState::Changed)
        );
        assert_eq!(module_tone(ModuleState::Failed), Tone::Danger);
        assert_eq!(module_tone(ModuleState::Unhealthy), Tone::Danger);
        assert_eq!(module_tone(ModuleState::Blocked), Tone::Warn);
        assert_eq!(module_tone(ModuleState::Indeterminate), Tone::Warn);
        assert_eq!(module_tone(ModuleState::RolledBack), Tone::Mute);
        assert_eq!(module_tone(ModuleState::Running), Tone::Active);
        assert_eq!(module_slug(ModuleState::NotStarted), "not-started");
        assert_eq!(run_tone(RunState::Succeeded), Tone::Ok);
        assert_eq!(run_tone(RunState::Failed), Tone::Danger);
        assert_eq!(run_tone(RunState::NeedsAttention), Tone::Warn);
        assert_eq!(run_label(RunState::StalePlan), "stale plan");
        assert!(!run_finished(RunState::Running));
        assert!(!run_finished(RunState::Pending));
        assert!(run_finished(RunState::RolledBack));
    }

    #[test]
    fn problems_and_the_running_module_open_by_default() {
        use ModuleState::*;
        for state in [
            Failed,
            Unhealthy,
            Blocked,
            Indeterminate,
            RolledBack,
            Running,
        ] {
            assert!(
                expanded_by_default(&module("m", state, vec![])),
                "{state:?} opens"
            );
        }
        for state in [Pending, Applied, Unchanged, NotStarted, Deferred, Skipped] {
            assert!(
                !expanded_by_default(&module("m", state, vec![])),
                "{state:?} stays collapsed"
            );
        }
        // A step that failed before the module's own state caught up.
        let early = module("m", Applied, vec![step("a", StepState::Failed)]);
        assert!(expanded_by_default(&early));
        let mut with_error = module("m", Applied, vec![]);
        with_error.error = Some(ErrorView::system("state not saved"));
        assert!(module_has_problem(&with_error));
    }

    #[test]
    fn the_current_step_is_the_running_one_handlers_last() {
        let mut m = module(
            "nomad",
            ModuleState::Running,
            vec![step("a", StepState::Changed), step("b", StepState::Pending)],
        );
        assert!(current_step(&m).is_none());
        m.handlers = vec![StepReport {
            handler: true,
            ..step("restart", StepState::Running)
        }];
        assert_eq!(current_step(&m).unwrap().id, "file.copy[restart]");
        m.steps[1].state = StepState::Running;
        assert_eq!(current_step(&m).unwrap().id, "file.copy[b]");
    }

    #[test]
    fn a_step_key_drops_the_kind_it_is_shown_beside() {
        assert_eq!(step_key("file.template", "file.template[/etc/x]"), "/etc/x");
        assert_eq!(step_key("apt.package", "apt.package[ufw, jq]"), "ufw, jq");
        assert_eq!(step_key("file.copy", "restart"), "restart");
        assert_eq!(step_key("file.copy", "file.copy[]"), "file.copy[]");
    }

    #[test]
    fn elapsed_time_prefers_what_was_reported() {
        let now = Utc::now();
        let ago = |s: i64| Some(now - chrono::Duration::seconds(s));
        assert_eq!(
            elapsed_ms(Some(1_500), ago(9), None, now, true),
            Some(1_500)
        );
        assert_eq!(elapsed_ms(None, ago(10), ago(4), now, false), Some(6_000));
        assert_eq!(elapsed_ms(None, ago(3), None, now, true), Some(3_000));
        assert_eq!(elapsed_ms(None, ago(3), None, now, false), None);
        assert_eq!(elapsed_ms(None, None, None, now, true), None);
    }

    #[test]
    fn progress_counts_every_step_that_has_stopped() {
        let r = report(vec![module(
            "m",
            ModuleState::Running,
            vec![
                step("a", StepState::Changed),
                step("b", StepState::Running),
                step("c", StepState::Pending),
                step("d", StepState::Failed),
            ],
        )]);
        assert_eq!(step_progress(&r), (2, 4));
    }
}
