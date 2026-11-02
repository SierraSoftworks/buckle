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
use crate::logic::run::{Applied, Watch, follow};
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
}

impl RunSource {
    fn watch(&self) -> Watch {
        match self {
            RunSource::Host { host, .. } => Watch {
                host: host.clone(),
                rollout: None,
                follow: false,
            },
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
    let fetch = {
        let (first, seed) = (first.clone(), seed.clone());
        use_fetch(source.clone(), move |source| {
            let seeded = std::mem::replace(&mut *first.borrow_mut(), false)
                .then_some(seed)
                .flatten();
            async move {
                if let Some(report) = seeded {
                    return Ok(Some(report));
                }
                match source {
                    None => Ok(None),
                    Some(RunSource::Host { host, run }) => {
                        api::host_run(&host, &run).await.map(Some)
                    }
                }
            }
        })
    };

    let mut report = match &fetch.data {
        Some(data) => data.1.clone(),
        None => seed,
    };
    let mut stale = false;
    if let (Some(data), Some(watch)) = (&fetch.data, source.as_ref().map(RunSource::watch))
        && let Some(events) = live.log.since(data.0)
    {
        for event in events {
            if let LiveEvent::RunProgress { progress } = &*event {
                stale |= follow(report.as_mut(), progress, &watch) == Applied::Refetch;
            }
        }
    }
    if let Some(r) = report.as_mut() {
        r.recount();
    }

    // Once per fetched answer: the next answer has a newer sequence number.
    let stale_at = stale.then(|| fetch.data.as_ref().map(|d| d.0)).flatten();
    {
        let reload = fetch.reload.clone();
        use_effect_with(stale_at, move |at| {
            if at.is_some() {
                reload.emit(());
            }
            || ()
        });
    }

    RunFeed {
        report,
        settled: fetch.data.is_some(),
        error: fetch.error.clone(),
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
