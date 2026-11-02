//! What a run report says, decided once: the colour, glyph and name of every
//! run, module and step state, which modules open by default, how long
//! things took, and what a host is doing right now.

use std::collections::BTreeMap;

use buckle_api::{
    LiveEvent, ModuleReport, ModuleState, RunProgress, RunReport, RunState, RunSummary, StepReport,
    StepState,
};
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

/// The tone of the error under a step or module of the given tone: a
/// blocked step's reason is a warning and a rolled-back module's cause takes
/// the rollback's colour; anything else is a failure.
pub fn error_tone(tone: Tone) -> Tone {
    match tone {
        Tone::Warn | Tone::Mute => tone,
        _ => Tone::Danger,
    }
}

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
/// `file.template[/etc/x]` → `/etc/x`, and a removal's resource without
/// its module too: `nomad/file.copy[/etc/x]` → `/etc/x`. An id of another
/// shape is kept whole.
pub fn step_key<'a>(kind: &str, id: &'a str) -> &'a str {
    let bare = |id: &'a str| {
        id.strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix('['))
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|key| !key.is_empty())
    };
    bare(id)
        .or_else(|| {
            id.split_once('/')
                .filter(|(module, _)| !module.contains('['))
                .and_then(|(_, rest)| bare(rest))
        })
        .unwrap_or(id)
}

/// The module of a removal's resource (`nomad/file.copy[/etc/x]` → `nomad`).
pub fn resource_module(resource: &str) -> Option<&str> {
    resource
        .split_once('/')
        .map(|(module, _)| module)
        .filter(|module| !module.is_empty() && !module.contains('['))
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

/// What a live update did to the report it was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The report now reflects it.
    Changed,
    /// About another host or another run: nothing to do here.
    Ignored,
    /// About this run, but it names a module or step the report does not
    /// hold (a deferred module was planned since, or updates were missed):
    /// the report has to be fetched again.
    Refetch,
}

/// Where in a report an update lands.
enum Target {
    Run,
    Module(usize),
    Task(usize, usize),
    Handler(usize, usize),
    Removal(usize),
}

fn target(report: &RunReport, p: &RunProgress) -> Option<Target> {
    // A removal is addressed by its resource (`module/kind[key]`), as the
    // step of its module or on its own.
    let removal = |id: &str| {
        report
            .removals
            .iter()
            .position(|r| r.resource == id)
            .map(Target::Removal)
    };
    match (&p.module, &p.step) {
        (None, None) => Some(Target::Run),
        (None, Some(step)) => removal(step),
        (Some(module), step) => {
            let Some(i) = report.modules.iter().position(|m| &m.name == module) else {
                return step
                    .as_ref()
                    .and_then(|s| removal(&format!("{module}/{s}")));
            };
            let Some(step) = step else {
                return Some(Target::Module(i));
            };
            let m = &report.modules[i];
            if let Some(j) = m.steps.iter().position(|s| &s.id == step) {
                Some(Target::Task(i, j))
            } else if let Some(j) = m.handlers.iter().position(|s| &s.id == step) {
                Some(Target::Handler(i, j))
            } else {
                removal(&format!("{module}/{step}")).or_else(|| removal(step))
            }
        }
    }
}

/// A fresh attempt (pending or running again) clears the error of the last
/// one unless the update brings its own.
fn fresh(step: StepState) -> bool {
    matches!(step, StepState::Pending | StepState::Running)
}

fn apply_step(s: &mut StepReport, p: &RunProgress) {
    if let Some(state) = p.step_state {
        match state {
            // Started (again): the last attempt's times are not this one's.
            StepState::Running if s.state != StepState::Running => {
                s.started_at = Some(p.at);
                s.finished_at = None;
                s.duration_ms = None;
            }
            StepState::Running => {}
            // Planned again: it has not run yet.
            StepState::Pending => {
                s.started_at = None;
                s.finished_at = None;
                s.duration_ms = None;
            }
            _ if s.finished_at.is_none() => s.finished_at = Some(p.at),
            _ => {}
        }
        s.state = state;
    }
    if let Some(result) = &p.result {
        s.result = Some(result.clone());
    }
    match &p.error {
        Some(e) => s.error = Some(e.clone()),
        None if fresh(s.state) => s.error = None,
        None => {}
    }
}

fn apply_module(m: &mut ModuleReport, p: &RunProgress, own_error: bool) {
    let Some(state) = p.module_state else {
        return;
    };
    match state {
        // Started (again), as for a step: a retry's time is its own.
        ModuleState::Running if m.state != ModuleState::Running => {
            m.started_at = Some(p.at);
            m.finished_at = None;
            m.duration_ms = None;
        }
        ModuleState::Running => {}
        ModuleState::Pending | ModuleState::Deferred => {
            m.started_at = None;
            m.finished_at = None;
            m.duration_ms = None;
        }
        _ if m.finished_at.is_none() => m.finished_at = Some(p.at),
        _ => {}
    }
    m.state = state;
    if own_error {
        match &p.error {
            Some(e) => m.error = Some(e.clone()),
            None if matches!(state, ModuleState::Pending | ModuleState::Running) => m.error = None,
            None => {}
        }
    }
}

/// Applies a live update for one step or module (or the run itself) to a
/// report: the state, result and error it carries, the times implied, the
/// module's change count, and the counts recomputed. An update for another
/// host or run is ignored; one naming something the report does not hold
/// asks for a refetch and leaves the report as it was.
pub fn apply_progress(report: &mut RunReport, p: &RunProgress) -> Applied {
    if p.host != report.host || p.run_id != report.id {
        return Applied::Ignored;
    }
    let Some(target) = target(report, p) else {
        return Applied::Refetch;
    };
    report.state = p.run_state;
    if p.at > report.updated_at {
        report.updated_at = p.at;
    }
    match target {
        Target::Run => {
            if let Some(e) = &p.error {
                report.error = Some(e.clone());
            }
        }
        Target::Module(i) => apply_module(&mut report.modules[i], p, true),
        Target::Task(i, j) | Target::Handler(i, j) => {
            let m = &mut report.modules[i];
            let step = if matches!(target, Target::Task(..)) {
                &mut m.steps[j]
            } else {
                &mut m.handlers[j]
            };
            apply_step(step, p);
            if p.module_state.is_some() {
                apply_module(m, p, false);
            } else if p.step_state == Some(StepState::Running)
                && matches!(
                    m.state,
                    ModuleState::Pending | ModuleState::NotStarted | ModuleState::Deferred
                )
            {
                // A step started before the module's own update arrived.
                m.state = ModuleState::Running;
                m.started_at = Some(p.at);
                m.finished_at = None;
                m.duration_ms = None;
            }
            m.changes = m
                .steps
                .iter()
                .filter(|s| s.state == StepState::Changed)
                .count() as u32;
        }
        Target::Removal(j) => {
            let r = &mut report.removals[j];
            if let Some(state) = p.step_state {
                r.state = state;
            }
            match &p.error {
                Some(e) => r.error = Some(e.clone()),
                None if fresh(r.state) => r.error = None,
                None => {}
            }
        }
    }
    if report.state == RunState::Running {
        report.started_at.get_or_insert(p.at);
    }
    if run_finished(report.state) && report.finished_at.is_none() {
        report.finished_at = Some(p.at);
    }
    report.recount();
    Applied::Changed
}

/// Which live updates a view of one run takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub host: String,
    /// Only the run of this rollout (a rollout page's host drawer): an
    /// update must name it, since a run outside any rollout (drift
    /// correction, a local rollback) on the same host names none.
    pub rollout: Option<String>,
    /// An update about another run of the host means the one held is no
    /// longer the one to show (a retry started a new run), so fetch again.
    pub follow: bool,
}

/// Offers an update to the report a view holds, or to a view that holds
/// none yet (the host had not started): applied, ignored, or a reason to
/// fetch the report again.
pub fn follow(held: Option<&mut RunReport>, p: &RunProgress, watch: &Watch) -> Applied {
    if p.host != watch.host {
        return Applied::Ignored;
    }
    if watch.rollout.is_some() && p.rollout_id != watch.rollout {
        return Applied::Ignored;
    }
    match held {
        Some(report) if report.id == p.run_id => apply_progress(report, p),
        _ if watch.follow => Applied::Refetch,
        _ => Applied::Ignored,
    }
}

/// The run a host page follows: the run of the newest update seen for the
/// host since the page loaded, else the latest run its detail named.
pub fn newest_run<'a>(
    held: Option<&str>,
    host: &str,
    events: impl IntoIterator<Item = &'a LiveEvent>,
) -> Option<String> {
    events
        .into_iter()
        .filter_map(|e| match e {
            LiveEvent::RunProgress { progress } if progress.host == host => {
                Some(progress.run_id.clone())
            }
            _ => None,
        })
        .last()
        .or_else(|| held.map(str::to_string))
}

/// Keeps a host's list of runs current from an update: the run's state
/// changes in place. Returns false when the run is not in the list (a new
/// run started), so the caller fetches the list again.
pub fn apply_run_list(list: &mut [RunSummary], host: &str, p: &RunProgress) -> bool {
    if p.host != host {
        return true;
    }
    match list.iter_mut().find(|r| r.id == p.run_id) {
        Some(run) => {
            run.state = Some(p.run_state);
            true
        }
        None => false,
    }
}

/// What a host is doing right now: the module it is in and, when known,
/// the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub module: String,
    pub step: Option<String>,
}

impl Activity {
    /// `nomad › file.template[/etc/nomad.d/nomad.hcl]`.
    pub fn describe(&self) -> String {
        match &self.step {
            Some(step) => format!("{} › {}", self.module, step),
            None => self.module.clone(),
        }
    }
}

/// The module a report is in and the step it is on, while it runs.
pub fn activity(r: &RunReport) -> Option<Activity> {
    if run_finished(r.state) {
        return None;
    }
    let module = r
        .modules
        .iter()
        .find(|m| m.state == ModuleState::Running)
        .or_else(|| r.modules.iter().find(|m| current_step(m).is_some()))?;
    Some(Activity {
        module: module.name.clone(),
        step: current_step(module).map(|s| s.id.clone()),
    })
}

/// What a host is doing after an update, from the updates alone (for a host
/// whose report is not held): a step or module starting is what it is on, and
/// it finishing leaves the rest.
pub fn track(current: Option<Activity>, p: &RunProgress) -> Option<Activity> {
    if run_finished(p.run_state) {
        return None;
    }
    let Some(module) = &p.module else {
        return current;
    };
    match (&p.step, p.step_state, p.module_state) {
        (Some(step), Some(StepState::Running), _) => Some(Activity {
            module: module.clone(),
            step: Some(step.clone()),
        }),
        (Some(step), Some(state), _) if state != StepState::Pending => current.map(|a| {
            if a.module == *module && a.step.as_deref() == Some(step) {
                Activity {
                    module: a.module,
                    step: None,
                }
            } else {
                a
            }
        }),
        (None, _, Some(ModuleState::Running)) => Some(Activity {
            module: module.clone(),
            step: None,
        }),
        (None, _, Some(_)) => current.filter(|a| a.module != *module),
        _ => current,
    }
}

/// What each host of a rollout is doing, described for its block: from the
/// reports held, kept current by the updates since, and from the updates
/// alone for hosts without one. (A plain list while it is built: a rollout
/// applies a handful of hosts at once, and the bundle is smaller for it.)
pub fn activities<'a>(
    mut reports: Vec<RunReport>,
    rollout: &str,
    events: impl IntoIterator<Item = &'a LiveEvent>,
) -> BTreeMap<String, String> {
    let mut tracked: Vec<(String, Option<Activity>)> = reports
        .iter()
        .map(|r| (r.host.clone(), activity(r)))
        .collect();
    for event in events {
        let LiveEvent::RunProgress { progress: p } = event else {
            continue;
        };
        if p.rollout_id.as_deref() != Some(rollout) {
            continue;
        }
        let slot = match tracked.iter().position(|(h, _)| *h == p.host) {
            Some(i) => i,
            None => {
                tracked.push((p.host.clone(), None));
                tracked.len() - 1
            }
        };
        let held = reports
            .iter_mut()
            .find(|r| r.host == p.host && r.id == p.run_id);
        let now = match held.map(|report| (apply_progress(report, p), report)) {
            Some((Applied::Changed, report)) => activity(report),
            _ => track(tracked[slot].1.take(), p),
        };
        tracked[slot].1 = now;
    }
    tracked
        .into_iter()
        .filter_map(|(host, a)| a.map(|a| (host, a.describe())))
        .collect()
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
    fn an_error_takes_the_tone_of_what_it_explains() {
        assert_eq!(error_tone(step_tone(StepState::Blocked)), Tone::Warn);
        assert_eq!(error_tone(module_tone(ModuleState::RolledBack)), Tone::Mute);
        assert_eq!(error_tone(step_tone(StepState::Failed)), Tone::Danger);
        assert_eq!(error_tone(step_tone(StepState::NotRun)), Tone::Danger);
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
        // A removal's resource names its module first.
        assert_eq!(
            step_key("file.copy", "nomad/file.copy[/etc/nomad.d/client.hcl]"),
            "/etc/nomad.d/client.hcl"
        );
        assert_eq!(
            resource_module("nomad/file.copy[/etc/nomad.d/client.hcl]"),
            Some("nomad")
        );
        // A key with slashes is not mistaken for a module.
        assert_eq!(step_key("file.copy", "file.copy[/etc/a/b]"), "/etc/a/b");
        assert_eq!(resource_module("file.copy[/etc/a/b]"), None);
        assert_eq!(
            step_key("file.copy", "other/apt.package[x]"),
            "other/apt.package[x]"
        );
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

    fn progress(module: Option<&str>, step: Option<&str>) -> RunProgress {
        RunProgress {
            host: "pi-1".into(),
            run_id: "run-1".into(),
            rollout_id: Some("42".into()),
            at: Utc::now(),
            run_state: RunState::Running,
            module: module.map(Into::into),
            module_state: None,
            step: step.map(Into::into),
            step_state: None,
            result: None,
            error: None,
        }
    }

    fn live_report() -> RunReport {
        let mut r = report(vec![
            module(
                "common",
                ModuleState::Applied,
                vec![step("a", StepState::Changed)],
            ),
            module(
                "nomad",
                ModuleState::Running,
                vec![step("b", StepState::Running), step("c", StepState::Pending)],
            ),
        ]);
        r.modules[1].handlers = vec![StepReport {
            handler: true,
            ..step("restart", StepState::Pending)
        }];
        r.removals = vec![buckle_api::RemovalReport {
            resource: "grey/file.copy[/etc/grey]".into(),
            kind: "file.copy".into(),
            summary: "delete /etc/grey".into(),
            state: StepState::Pending,
            caution: false,
            error: None,
        }];
        r.recount();
        r
    }

    #[test]
    fn an_update_changes_its_step_and_recounts() {
        let mut r = live_report();
        assert_eq!((r.counts.steps.running, r.counts.steps.pending), (1, 3));
        let mut p = progress(Some("nomad"), Some("file.copy[b]"));
        p.step_state = Some(StepState::Changed);
        p.result = Some("copied".into());
        assert_eq!(apply_progress(&mut r, &p), Applied::Changed);
        let b = &r.modules[1].steps[0];
        assert_eq!(b.state, StepState::Changed);
        assert_eq!(b.result.as_deref(), Some("copied"));
        assert_eq!(b.finished_at, Some(p.at));
        assert_eq!(r.modules[1].changes, 1);
        assert_eq!(r.counts.steps.changed, 2);
        assert_eq!(r.counts.steps.running, 0);
        // The other steps are untouched.
        assert_eq!(r.modules[1].steps[1].state, StepState::Pending);
        assert_eq!(r.modules[0].steps[0].state, StepState::Changed);

        // Applying it again changes nothing.
        let before = r.clone();
        apply_progress(&mut r, &p);
        assert_eq!(r, before);
    }

    #[test]
    fn a_failure_brings_its_error_and_a_retry_clears_it() {
        let mut r = live_report();
        let mut p = progress(Some("nomad"), Some("file.copy[c]"));
        p.step_state = Some(StepState::Failed);
        p.error = Some(ErrorView::user("no space left"));
        p.module_state = Some(ModuleState::Failed);
        p.run_state = RunState::Failed;
        apply_progress(&mut r, &p);
        assert_eq!(
            r.modules[1].steps[1].error.as_ref().unwrap().message,
            "no space left"
        );
        assert_eq!(r.modules[1].state, ModuleState::Failed);
        assert!(
            r.modules[1].error.is_none(),
            "the step's error is the step's"
        );
        assert_eq!((r.state, r.finished_at), (RunState::Failed, Some(p.at)));
        assert_eq!(r.counts.modules.failed, 1);

        p.step_state = Some(StepState::Running);
        p.error = None;
        p.module_state = Some(ModuleState::Running);
        p.run_state = RunState::Running;
        apply_progress(&mut r, &p);
        assert!(r.modules[1].steps[1].error.is_none());
        assert_eq!(r.modules[1].steps[1].started_at, Some(p.at));
    }

    #[test]
    fn a_retry_times_itself_afresh() {
        let mut r = live_report();
        let earlier = Utc::now() - chrono::Duration::minutes(5);
        {
            let m = &mut r.modules[1];
            m.state = ModuleState::Failed;
            m.started_at = Some(earlier);
            m.finished_at = Some(earlier + chrono::Duration::seconds(40));
            m.duration_ms = Some(40_000);
            let s = &mut m.steps[0];
            s.state = StepState::Failed;
            s.started_at = Some(earlier);
            s.finished_at = Some(earlier + chrono::Duration::seconds(30));
            s.duration_ms = Some(30_000);
        }
        // The module is retried: its own times start again, so a duration
        // computed while it runs is this attempt's, not the last one's.
        let mut p = progress(Some("nomad"), None);
        p.module_state = Some(ModuleState::Running);
        apply_progress(&mut r, &p);
        let m = &r.modules[1];
        assert_eq!(m.started_at, Some(p.at));
        assert_eq!((m.finished_at, m.duration_ms), (None, None));
        assert_eq!(
            elapsed_ms(m.duration_ms, m.started_at, m.finished_at, p.at, true),
            Some(0)
        );

        // Its failed step is planned again, then runs again.
        let mut q = progress(Some("nomad"), Some("file.copy[b]"));
        q.step_state = Some(StepState::Pending);
        apply_progress(&mut r, &q);
        let s = &r.modules[1].steps[0];
        assert_eq!(
            (s.started_at, s.finished_at, s.duration_ms),
            (None, None, None)
        );
        q.step_state = Some(StepState::Running);
        apply_progress(&mut r, &q);
        assert_eq!(r.modules[1].steps[0].started_at, Some(q.at));

        // A module planned again has not started.
        let mut d = progress(Some("nomad"), None);
        d.module_state = Some(ModuleState::Pending);
        apply_progress(&mut r, &d);
        assert_eq!(r.modules[1].started_at, None);
    }

    #[test]
    fn module_handler_removal_and_run_updates_land_where_they_belong() {
        let mut r = live_report();
        let mut p = progress(Some("nomad"), Some("file.copy[restart]"));
        p.step_state = Some(StepState::Running);
        apply_progress(&mut r, &p);
        assert_eq!(r.modules[1].handlers[0].state, StepState::Running);

        let mut p = progress(Some("nomad"), None);
        p.module_state = Some(ModuleState::Unhealthy);
        p.error = Some(ErrorView::system("nomad/api failed"));
        apply_progress(&mut r, &p);
        assert_eq!(r.modules[1].state, ModuleState::Unhealthy);
        assert!(r.modules[1].error.is_some());

        let mut p = progress(None, Some("grey/file.copy[/etc/grey]"));
        p.step_state = Some(StepState::Changed);
        assert_eq!(apply_progress(&mut r, &p), Applied::Changed);
        assert_eq!(r.removals[0].state, StepState::Changed);
        // Or as the step of its module.
        let mut p = progress(Some("grey"), Some("file.copy[/etc/grey]"));
        p.step_state = Some(StepState::Failed);
        assert_eq!(apply_progress(&mut r, &p), Applied::Changed);
        assert_eq!(r.removals[0].state, StepState::Failed);

        let mut p = progress(None, None);
        p.run_state = RunState::StalePlan;
        p.error = Some(ErrorView::user("the host planned differently"));
        apply_progress(&mut r, &p);
        assert_eq!(r.state, RunState::StalePlan);
        assert!(r.error.is_some() && r.finished_at.is_some());
    }

    #[test]
    fn a_step_starting_starts_its_pending_module() {
        let mut r = live_report();
        r.modules[0].state = ModuleState::Pending;
        let mut p = progress(Some("common"), Some("file.copy[a]"));
        p.step_state = Some(StepState::Running);
        apply_progress(&mut r, &p);
        assert_eq!(r.modules[0].state, ModuleState::Running);
    }

    #[test]
    fn updates_for_another_host_or_run_are_ignored() {
        let mut r = live_report();
        let before = r.clone();
        let mut p = progress(Some("nomad"), Some("file.copy[b]"));
        p.step_state = Some(StepState::Failed);
        p.run_id = "run-2".into();
        assert_eq!(apply_progress(&mut r, &p), Applied::Ignored);
        p.run_id = "run-1".into();
        p.host = "pi-2".into();
        assert_eq!(apply_progress(&mut r, &p), Applied::Ignored);
        assert_eq!(r, before);
    }

    #[test]
    fn an_unknown_module_or_step_asks_for_a_refetch_and_changes_nothing() {
        let mut r = live_report();
        let before = r.clone();
        let mut p = progress(Some("otel"), Some("apt.package[otel]"));
        p.step_state = Some(StepState::Running);
        assert_eq!(apply_progress(&mut r, &p), Applied::Refetch);
        let mut p = progress(Some("nomad"), Some("file.copy[zz]"));
        p.step_state = Some(StepState::Running);
        p.run_state = RunState::Failed;
        assert_eq!(apply_progress(&mut r, &p), Applied::Refetch);
        assert_eq!(r, before);
    }

    #[test]
    fn a_view_follows_only_its_host_and_rollout() {
        let watch = Watch {
            host: "pi-1".into(),
            rollout: Some("42".into()),
            follow: true,
        };
        let mut r = live_report();
        let mut p = progress(Some("nomad"), Some("file.copy[b]"));
        p.step_state = Some(StepState::Changed);
        assert_eq!(follow(Some(&mut r), &p, &watch), Applied::Changed);
        // A retry started another run of this rollout: fetch it.
        p.run_id = "run-2".into();
        assert_eq!(follow(Some(&mut r), &p, &watch), Applied::Refetch);
        // The host had no run yet: now it has one.
        assert_eq!(follow(None, &p, &watch), Applied::Refetch);
        // Another rollout's run, or another host's, is not this view's, and
        // nor is a run outside any rollout on the same host.
        p.rollout_id = Some("43".into());
        assert_eq!(follow(None, &p, &watch), Applied::Ignored);
        p.rollout_id = None;
        assert_eq!(follow(None, &p, &watch), Applied::Ignored);
        assert_eq!(follow(Some(&mut r), &p, &watch), Applied::Ignored);
        p.rollout_id = Some("42".into());
        p.host = "pi-2".into();
        assert_eq!(follow(None, &p, &watch), Applied::Ignored);
        // A view pinned to one run does not chase others.
        let pinned = Watch {
            rollout: None,
            follow: false,
            ..watch
        };
        let mut p = progress(None, None);
        p.run_id = "run-2".into();
        assert_eq!(follow(Some(&mut r), &p, &pinned), Applied::Ignored);
    }

    #[test]
    fn a_host_page_follows_the_newest_run_it_has_heard_of() {
        let event = |host: &str, run: &str| LiveEvent::RunProgress {
            progress: RunProgress {
                host: host.into(),
                run_id: run.into(),
                ..progress(None, None)
            },
        };
        let none: Vec<LiveEvent> = vec![];
        assert_eq!(
            newest_run(Some("run-1"), "pi-1", &none).as_deref(),
            Some("run-1")
        );
        assert_eq!(newest_run(None, "pi-1", &none), None);
        let events = [event("pi-1", "run-2"), event("pi-2", "run-9")];
        assert_eq!(
            newest_run(Some("run-1"), "pi-1", &events).as_deref(),
            Some("run-2")
        );
        assert_eq!(newest_run(None, "pi-3", &events), None);
    }

    #[test]
    fn a_run_list_takes_new_states_and_notices_new_runs() {
        let now = Utc::now();
        let summary = |id: &str| RunSummary {
            id: id.into(),
            state: Some(RunState::Running),
            revision: "r".into(),
            plan_digest: "d".into(),
            started_at: now,
            finished_at: now,
            outcome: "running".into(),
            changes: 0,
            removals: 0,
            error: None,
            trace_id: None,
            trace_url: None,
        };
        let mut list = vec![summary("run-1")];
        let mut p = progress(None, None);
        p.run_state = RunState::Succeeded;
        assert!(apply_run_list(&mut list, "pi-1", &p));
        assert_eq!(list[0].state, Some(RunState::Succeeded));
        p.run_id = "run-2".into();
        assert!(!apply_run_list(&mut list, "pi-1", &p), "a new run");
        assert!(apply_run_list(&mut list, "pi-2", &p), "not this host's");
    }

    #[test]
    fn a_running_report_says_where_it_is() {
        let r = live_report();
        let a = activity(&r).unwrap();
        assert_eq!(a.describe(), "nomad › file.copy[b]");
        let mut done = r.clone();
        done.state = RunState::Succeeded;
        assert_eq!(activity(&done), None);
        let mut between = r;
        between.modules[1].steps[0].state = StepState::Changed;
        assert_eq!(activity(&between).unwrap().describe(), "nomad");
    }

    #[test]
    fn updates_alone_track_the_step_in_progress() {
        let mut p = progress(Some("nomad"), Some("apt.package[nomad]"));
        p.step_state = Some(StepState::Running);
        let a = track(None, &p);
        assert_eq!(a.as_ref().unwrap().describe(), "nomad › apt.package[nomad]");
        p.step_state = Some(StepState::Changed);
        let a = track(a, &p);
        assert_eq!(a.as_ref().unwrap().describe(), "nomad");
        let mut m = progress(Some("nomad"), None);
        m.module_state = Some(ModuleState::Applied);
        assert_eq!(track(a.clone(), &m), None);
        let mut end = progress(None, None);
        end.run_state = RunState::Succeeded;
        assert_eq!(track(a, &end), None);
    }

    #[test]
    fn a_rollouts_hosts_report_what_they_are_doing() {
        let held = live_report();
        let event = |host: &str, run: &str, rollout: &str, step: &str| LiveEvent::RunProgress {
            progress: RunProgress {
                host: host.into(),
                run_id: run.into(),
                rollout_id: Some(rollout.into()),
                step_state: Some(StepState::Running),
                ..progress(Some("nomad"), Some(step))
            },
        };
        let mut local = event("pi-4", "run-9", "", "apt.package[nomad]");
        if let LiveEvent::RunProgress { progress } = &mut local {
            progress.rollout_id = None;
        }
        let events = [
            event("pi-1", "run-1", "42", "file.copy[c]"),
            event("pi-2", "run-7", "42", "apt.package[nomad]"),
            event("pi-3", "run-8", "41", "apt.package[nomad]"),
            local,
        ];
        let map = activities(vec![held], "42", &events);
        // pi-1's held report was moved on by the update: two steps running,
        // the first is named.
        assert_eq!(map["pi-1"], "nomad › file.copy[b]");
        assert_eq!(map["pi-2"], "nomad › apt.package[nomad]");
        assert!(!map.contains_key("pi-3"), "another rollout's host");
        assert!(!map.contains_key("pi-4"), "a run outside any rollout");
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
