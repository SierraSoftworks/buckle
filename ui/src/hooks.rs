//! Fetching from the API, keeping it live, and the clock.
//!
//! Every page does the same things with what it loads: fetch on mount, say
//! so while it waits, show the error when it fails with a way to try again,
//! and keep what it has when a reload fails rather than emptying itself.
//! Written once here, those are the same on every page.

use std::future::Future;
use std::rc::Rc;

use buckle_api::{LiveEvent, RunReport};
use chrono::{DateTime, Utc};
use gloo_timers::callback::{Interval, Timeout};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api::{self, ApiError};
use crate::logic::run::{Applied, RefetchPolicy, Watch, follow, refetch_key};
use crate::store::use_live;

/// The state of one fetched resource.
pub struct Fetch<T> {
    /// What was fetched, once something has been, and the live store's
    /// sequence number when the fetch began.
    pub data: Option<Rc<(u64, T)>>,
    pub error: Option<ApiError>,
    pub loading: bool,
    pub reload: Callback<()>,
    /// Replaces the data, e.g. with what an action's response returned.
    pub set: Callback<T>,
}

impl<T> Fetch<T> {
    pub fn value(&self) -> Option<&T> {
        self.data.as_deref().map(|(_, v)| v)
    }
}

/// Fetches on mount, whenever `deps` change, when [`Fetch::reload`] is
/// called, and whenever the live store says events may have been missed.
#[hook]
pub fn use_fetch<T, D, F, Fut>(deps: D, fetch: F) -> Fetch<T>
where
    T: 'static,
    D: PartialEq + Clone + 'static,
    F: FnOnce(D) -> Fut + 'static,
    Fut: Future<Output = Result<T, ApiError>> + 'static,
{
    let live = use_live();
    let data = use_state(|| None::<Rc<(u64, T)>>);
    let error = use_state(|| None::<ApiError>);
    let loading = use_state(|| true);
    let generation = use_state(|| 0u32);
    // Only the newest request may write: a slow answer to an old question
    // must not overwrite the answer to the current one.
    let latest = use_mut_ref(|| 0u64);
    let seq = live.log.seq;

    {
        let (data, error, loading, latest) =
            (data.clone(), error.clone(), loading.clone(), latest.clone());
        use_effect_with((deps, live.epoch, *generation), move |(deps, _, _)| {
            let ticket = {
                let mut l = latest.borrow_mut();
                *l += 1;
                *l
            };
            loading.set(true);
            let future = fetch(deps.clone());
            spawn_local(async move {
                let result = future.await;
                if *latest.borrow() != ticket {
                    return;
                }
                match result {
                    Ok(value) => {
                        data.set(Some(Rc::new((seq, value))));
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)),
                }
                loading.set(false);
            });
            || ()
        });
    }

    // Events were dropped from the log before this page saw them.
    {
        let generation = generation.clone();
        let base = data.as_ref().map(|d| d.0);
        let gap = base.is_some_and(|b| live.log.since(b).is_none());
        use_effect_with(gap, move |gap| {
            if *gap {
                generation.set(*generation + 1);
            }
            || ()
        });
    }

    let reload = {
        let generation = generation.clone();
        Callback::from(move |_| generation.set(*generation + 1))
    };
    let set = {
        let data = data.clone();
        Callback::from(move |value: T| data.set(Some(Rc::new((seq, value)))))
    };

    Fetch {
        data: (*data).clone(),
        error: (*error).clone(),
        loading: *loading,
        reload,
        set,
    }
}

/// The fetched value with every live event since applied to it.
#[hook]
pub fn use_live_value<T>(fetch: &Fetch<T>, apply: fn(&mut T, &LiveEvent)) -> Option<T>
where
    T: Clone + 'static,
{
    let live = use_live();
    let data = fetch.data.clone()?;
    let mut value = data.1.clone();
    if let Some(events) = live.log.since(data.0) {
        for event in events {
            apply(&mut value, &event);
        }
    }
    Some(value)
}

/// Where a run report comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSource {
    /// One run of a host: `GET /hosts/{host}/runs/{run}`.
    Host { host: String, run: String },
    /// The host's run in a rollout: `GET /rollouts/{rollout}/hosts/{host}/report`.
    /// `run` is the entry's `run_id`, so that a new run (a retry) is fetched.
    Rollout {
        rollout: String,
        host: String,
        run: Option<String>,
    },
}

impl RunSource {
    fn watch(&self) -> Watch {
        match self {
            RunSource::Host { host, .. } => Watch {
                host: host.clone(),
                rollout: None,
                follow: false,
            },
            // A placement (`cluster/workload`) shares its cluster's updates
            // with the cluster's other placements, so another run there is
            // no reason to fetch again: a retry changes `run` instead.
            RunSource::Rollout { rollout, host, .. } => {
                let (target, workload) = buckle_api::split_placement_key(host);
                Watch {
                    host: target.to_owned(),
                    rollout: Some(rollout.clone()),
                    follow: workload.is_none(),
                }
            }
        }
    }
}

/// A run report, kept current.
pub struct RunFeed {
    /// The report with every update since it was fetched applied; `None`
    /// while loading, or when there is no run (yet).
    pub report: Option<RunReport>,
    /// The source has answered, with a report or with "no run yet".
    pub settled: bool,
    pub error: Option<ApiError>,
    pub reload: Callback<()>,
}

/// A fetched answer and the source it answers: a report, or `None` for "no
/// run yet". An answer counts only for the source it was asked for, so a
/// "no run yet" from one source does not answer the next.
type Answer = (Option<RunSource>, Option<RunReport>);

/// Of what a fetch holds, what belongs to the source asked about now: the
/// answer when it was given for this source, and the error only once this
/// source's own request has failed (a fetch keeps its last error until a
/// request succeeds, so an earlier source's error would otherwise show
/// while this one loads).
/// What a fetch holds: the live store's sequence number when it was asked,
/// and the answer tagged with its source.
type Tagged<S, R> = (u64, (S, R));

fn current<'a, S: PartialEq, R>(
    source: &S,
    data: Option<&'a Tagged<S, R>>,
    error: Option<&'a ApiError>,
    asked: Option<&S>,
    loading: bool,
) -> (Option<&'a Tagged<S, R>>, Option<&'a ApiError>) {
    let answer = data.filter(|d| d.1.0 == *source);
    let error = error.filter(|_| !loading && asked == Some(source));
    (answer, error)
}

/// Fetches a run report and applies the `run_progress` updates that follow.
/// It fetches again when an update names something the report does not hold
/// (or, for a rollout's host, a run other than the one held), and, like
/// every fetch, when the stream lagged or reconnected.
///
/// `seed` is the report the page already holds for this run (a host's
/// detail carries its latest run, kept current by the same updates): it
/// answers the first fetch, so the report is not asked for twice on load.
#[hook]
pub fn use_run_report(source: Option<RunSource>, seed: Option<RunReport>) -> RunFeed {
    let live = use_live();
    let seed =
        seed.filter(|s| matches!(&source, Some(RunSource::Host { run, .. }) if *run == s.id));
    let first = use_mut_ref(|| true);
    // The source of the newest request, set as it is sent.
    let asked = use_mut_ref(|| None::<Option<RunSource>>);
    let fetch = {
        let (first, asked, seed) = (first.clone(), asked.clone(), seed.clone());
        use_fetch(source.clone(), move |source| {
            *asked.borrow_mut() = Some(source.clone());
            let seeded = std::mem::replace(&mut *first.borrow_mut(), false)
                .then_some(seed)
                .flatten();
            async move {
                let report = match (&source, seeded) {
                    (_, Some(report)) => Some(report),
                    (None, None) => None,
                    (Some(RunSource::Host { host, run }), None) => {
                        Some(api::host_run(host, run).await?)
                    }
                    (Some(RunSource::Rollout { rollout, host, .. }), None) => {
                        match api::rollout_host_report(rollout, host).await {
                            Ok(report) => Some(report),
                            // No run in this rollout yet.
                            Err(ApiError::NotFound(_)) => None,
                            Err(e) => return Err(e),
                        }
                    }
                };
                Ok::<Answer, ApiError>((source, report))
            }
        })
    };

    let asked_for = asked.borrow().clone();
    let (answer, error) = current(
        &source,
        fetch.data.as_deref(),
        fetch.error.as_ref(),
        asked_for.as_ref(),
        fetch.loading,
    );
    let mut report = match answer {
        Some((_, (_, report))) => report.clone(),
        None => seed,
    };
    // The first thing an update asked about that the report could not take.
    let mut question: Option<String> = None;
    if let (Some((seq, _)), Some(watch)) = (answer, source.as_ref().map(RunSource::watch))
        && let Some(events) = live.log.since(*seq)
    {
        for event in events {
            if let LiveEvent::RunProgress { progress } = &*event
                && follow(report.as_mut(), progress, &watch) == Applied::Refetch
                && question.is_none()
            {
                question = Some(refetch_key(report.as_ref(), progress));
            }
        }
    }
    if let Some(r) = report.as_mut() {
        r.recount();
    }

    // Fetch again for it: never while a fetch is in flight, and for the
    // same question as the last fetch (whose answer did not have it yet)
    // only after a growing wait, so a server slow to learn of a new step is
    // not asked once per update.
    let policy = use_mut_ref(RefetchPolicy::default);
    {
        let (reload, policy) = (fetch.reload.clone(), policy.clone());
        let at = answer.map(|d| d.0);
        use_effect_with(
            (at, question, fetch.loading),
            move |(_, question, loading)| {
                let wait = policy.borrow_mut().next(question.as_deref(), *loading);
                let timer = wait.map(|ms| Timeout::new(ms, move || reload.emit(())));
                move || drop(timer)
            },
        );
    }

    RunFeed {
        report,
        settled: answer.is_some(),
        error: error.cloned(),
        reload: fetch.reload.clone(),
    }
}

/// The current time, ticking every `secs` seconds.
#[hook]
pub fn use_now(secs: u32) -> DateTime<Utc> {
    let now = use_state(Utc::now);
    {
        let now = now.clone();
        use_effect_with(secs, move |secs| {
            let interval = Interval::new(secs * 1_000, move || now.set(Utc::now()));
            move || drop(interval)
        });
    }
    *now
}

/// `value`, once it has stopped changing for `ms` milliseconds.
#[hook]
pub fn use_debounced<T>(value: T, ms: u32) -> T
where
    T: Clone + PartialEq + 'static,
{
    let settled = use_state(|| value.clone());
    {
        let settled = settled.clone();
        use_effect_with(value, move |value| {
            let value = value.clone();
            let timeout = Timeout::new(ms, move || settled.set(value));
            move || drop(timeout)
        });
    }
    (*settled).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(
        seq: u64,
        source: &'static str,
        report: Option<u32>,
    ) -> (u64, (&'static str, Option<u32>)) {
        (seq, (source, report))
    }

    #[test]
    fn an_answer_counts_only_for_the_source_it_was_asked_for() {
        // "No run yet" for run A is not an answer about run B.
        let none_for_a = answer(3, "a", None);
        let (got, _) = current(&"b", Some(&none_for_a), None, Some(&"b"), true);
        assert_eq!(got, None, "B is still loading");
        let (got, _) = current(&"a", Some(&none_for_a), None, Some(&"a"), false);
        assert_eq!(got, Some(&none_for_a));
        let report_for_b = answer(4, "b", Some(7));
        let (got, _) = current(&"b", Some(&report_for_b), None, Some(&"b"), false);
        assert_eq!(got.map(|a| a.1.1), Some(Some(7)));
    }

    #[test]
    fn an_error_shows_only_once_this_sources_request_failed() {
        let failed = ApiError::Server("boom".into());
        // The render right after the source changed: the request for B has
        // not been sent yet, and the error is A's.
        assert_eq!(
            current::<_, ()>(&"b", None, Some(&failed), Some(&"a"), false).1,
            None
        );
        // B's request is on its way.
        assert_eq!(
            current::<_, ()>(&"b", None, Some(&failed), Some(&"b"), true).1,
            None
        );
        // B's request failed.
        assert_eq!(
            current::<_, ()>(&"b", None, Some(&failed), Some(&"b"), false).1,
            Some(&failed)
        );
    }
}
