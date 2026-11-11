//! The Rollouts timeline: the configuration repository's commits with
//! operators' actions between them, newest first. A commit stands where its
//! rollout started (where it was committed, if it has none yet); an action
//! where it started. `Show` narrows it to the commits or the actions.

use buckle_api::{ActionRun, LiveEvent, RevisionSummary, RolloutSummary};
use chrono::{DateTime, Utc};

use super::run::run_finished;

/// Which entries the timeline shows (the page head's toggle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Show {
    #[default]
    All,
    Repo,
    Actions,
}

impl Show {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("repo") => Show::Repo,
            Some("actions") => Show::Actions,
            _ => Show::All,
        }
    }

    /// Its value in the URL (`?show=`); none for All.
    pub fn as_query(self) -> Option<&'static str> {
        match self {
            Show::All => None,
            Show::Repo => Some("repo"),
            Show::Actions => Some("actions"),
        }
    }
}

/// One entry of the timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry<'a> {
    /// A commit, with its rollout where that is loaded.
    Commit(&'a RevisionSummary, Option<&'a RolloutSummary>),
    /// A rollout of an action across the fleet.
    Rollout(&'a RolloutSummary),
    /// An action run on one target.
    Run(&'a ActionRun),
}

impl Entry<'_> {
    /// Where the entry stands on the timeline.
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Entry::Commit(_, Some(x)) => x.started_at,
            Entry::Commit(r, None) => r.created_at,
            Entry::Rollout(x) => x.started_at,
            Entry::Run(r) => r.started_at,
        }
    }
}

/// The entries `show` asks for, newest first. Anything older than `since`
/// is left out: a list with older pages still to load cannot say what else
/// happened before its oldest entry.
pub fn entries<'a>(
    revisions: &'a [RevisionSummary],
    rollouts: &'a [RolloutSummary],
    runs: &'a [ActionRun],
    show: Show,
    since: Option<DateTime<Utc>>,
) -> Vec<Entry<'a>> {
    let mut out: Vec<Entry<'a>> = Vec::new();
    if show != Show::Actions {
        out.extend(revisions.iter().map(|r| {
            let rollout = r
                .rollout_id
                .as_ref()
                .and_then(|id| rollouts.iter().find(|x| &x.id == id));
            Entry::Commit(r, rollout)
        }));
    }
    if show != Show::Repo {
        out.extend(
            rollouts
                .iter()
                .filter(|x| x.action.is_some())
                .map(Entry::Rollout),
        );
        out.extend(runs.iter().map(Entry::Run));
    }
    out.retain(|e| since.is_none_or(|s| e.at() >= s));
    // Stable, so commits keep their order where their times tie.
    out.sort_by_key(|e| std::cmp::Reverse(e.at()));
    out
}

/// The oldest moment every list still to be paged is complete back to:
/// the newest of their oldest entries.
pub fn complete_since(
    pages: impl IntoIterator<Item = (bool, Option<DateTime<Utc>>)>,
) -> Option<DateTime<Utc>> {
    pages
        .into_iter()
        .filter_map(|(more, oldest)| more.then_some(oldest).flatten())
        .max()
}

/// Keeps action runs current: a run's progress moves its state on and,
/// once it ends, sets when it finished.
pub fn apply_action_runs(list: &mut [ActionRun], event: &LiveEvent) {
    if let LiveEvent::RunProgress { progress } = event
        && let Some(r) = list.iter_mut().find(|r| r.id == progress.run_id)
    {
        r.state = progress.run_state;
        if run_finished(progress.run_state) {
            r.finished_at.get_or_insert(progress.at);
        }
    }
}

/// Whether `event` says an action run started somewhere: the list is
/// fetched again to show it.
pub fn starts_action_run(event: &LiveEvent) -> bool {
    matches!(event, LiveEvent::Audit { entry } if entry.action.ends_with(".action.run"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{
        AuditEntryRef, RevisionStatus, RolloutActionInfo, RolloutState, RunProgress, RunState,
        TargetKind,
    };
    use chrono::Duration;

    fn t(mins: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::minutes(mins)
    }

    fn revision(id: &str, at: i64, rollout: Option<&str>) -> RevisionSummary {
        RevisionSummary {
            id: id.into(),
            commit: id.into(),
            author: "a".into(),
            message: "m".into(),
            created_at: t(at),
            status: RevisionStatus::Valid,
            validation_errors: vec![],
            is_stable: false,
            rollout_id: rollout.map(Into::into),
            module_count: 1,
            workload_count: 0,
        }
    }

    fn rollout(id: &str, at: i64, action: bool) -> RolloutSummary {
        RolloutSummary {
            id: id.into(),
            name: None,
            revision: "r".into(),
            revision_message: String::new(),
            state: RolloutState::Completed,
            started_at: t(at),
            finished_at: None,
            triggered_by: "alice".into(),
            hosts_total: 1,
            hosts_done: 1,
            hosts_failed: 0,
            current_phase: None,
            pause_reason: None,
            end_reason: None,
            superseded_by: None,
            trace_id: None,
            trace_url: None,
            action: action.then(|| RolloutActionInfo {
                module: "m".into(),
                action: "a".into(),
                label: "A".into(),
                selector: None,
                instances: None,
            }),
            changes: None,
        }
    }

    fn run(id: &str, at: i64) -> ActionRun {
        ActionRun {
            id: id.into(),
            target: "pi-1".into(),
            kind: TargetKind::Host,
            module: "m".into(),
            action: "a".into(),
            label: None,
            actor: None,
            state: RunState::Running,
            started_at: t(at),
            finished_at: None,
            error: None,
            trace_url: None,
        }
    }

    fn ids(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Commit(r, _) => r.id.clone(),
                Entry::Rollout(x) => x.id.clone(),
                Entry::Run(r) => r.id.clone(),
            })
            .collect()
    }

    struct Fixture {
        revisions: Vec<RevisionSummary>,
        rollouts: Vec<RolloutSummary>,
        runs: Vec<ActionRun>,
    }

    /// c1 committed at 0 and rolled out at 30; c2 committed at 20, never
    /// rolled out; an action rollout at 25 and a run at 10.
    fn fixture() -> Fixture {
        Fixture {
            revisions: vec![revision("c2", 20, None), revision("c1", 0, Some("ro1"))],
            rollouts: vec![rollout("ro1", 30, false), rollout("act", 25, true)],
            runs: vec![run("run", 10)],
        }
    }

    #[test]
    fn actions_stand_between_commits_by_when_their_rollouts_started() {
        let f = fixture();
        let cases = [
            (Show::All, None, vec!["c1", "act", "c2", "run"]),
            (Show::Repo, None, vec!["c1", "c2"]),
            (Show::Actions, None, vec!["act", "run"]),
            (Show::All, Some(t(15)), vec!["c1", "act", "c2"]),
        ];
        for (show, since, want) in cases {
            assert_eq!(
                ids(&entries(&f.revisions, &f.rollouts, &f.runs, show, since)),
                want,
                "{show:?} since {since:?}"
            );
        }
    }

    #[test]
    fn a_commit_without_its_rollout_loaded_stands_where_it_was_committed() {
        let f = fixture();
        let found = entries(&f.revisions, &[], &f.runs, Show::All, None);
        assert_eq!(ids(&found), vec!["c2", "run", "c1"]);
    }

    #[test]
    fn only_lists_with_more_pages_bound_the_timeline() {
        let cases = [
            (vec![(false, Some(t(5))), (false, Some(t(9)))], None),
            (vec![(true, Some(t(5))), (false, Some(t(9)))], Some(t(5))),
            (vec![(true, Some(t(5))), (true, Some(t(9)))], Some(t(9))),
            (vec![(true, None)], None),
        ];
        for (pages, want) in cases {
            assert_eq!(complete_since(pages.clone()), want, "{pages:?}");
        }
    }

    #[test]
    fn a_runs_progress_finishes_it_and_a_started_action_is_noticed() {
        let mut list = vec![run("run", 0)];
        let progress = |state| LiveEvent::RunProgress {
            progress: RunProgress {
                host: "pi-1".into(),
                run_id: "run".into(),
                rollout_id: None,
                at: t(3),
                run_state: state,
                module: None,
                module_state: None,
                step: None,
                step_state: None,
                result: None,
                error: None,
            },
        };
        apply_action_runs(&mut list, &progress(RunState::Running));
        assert_eq!(
            (list[0].state, list[0].finished_at),
            (RunState::Running, None)
        );
        apply_action_runs(&mut list, &progress(RunState::Failed));
        assert_eq!(
            (list[0].state, list[0].finished_at),
            (RunState::Failed, Some(t(3)))
        );

        let audit = |action: &str| LiveEvent::Audit {
            entry: AuditEntryRef {
                id: 1,
                at: t(0),
                action: action.into(),
                message: String::new(),
            },
        };
        assert!(starts_action_run(&audit("host.action.run")));
        assert!(starts_action_run(&audit("target.action.run")));
        assert!(!starts_action_run(&audit("rollout.start")));
    }
}
