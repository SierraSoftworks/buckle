//! Rollouts (design §12): the configuration repository's commits, newest
//! first, each with how its rollout is going and the one control that fits
//! where it stands: [Stable], Rollback, Review or [Running]. Revisions and
//! rollouts are one history, so there is one page for both, and operators'
//! actions (rolled out across the fleet, or run on one target) stand between
//! the commits on the same timeline. The head's toggle shows all of it, only
//! the repository's commits, or only the actions.

use buckle_api::{LiveEvent, Page};
use serde::{Deserialize, Serialize};
use yew::prelude::*;
use yew_router::prelude::{use_location, use_navigator};

use crate::api;
use crate::app::Route;
use crate::components::timeline::Timeline;
use crate::components::{
    Button, Empty, ErrorState, Loading, PageHead, Panel, Refresh, Segmented, use_revision_actions,
};
use crate::hooks::{Fetch, use_fetch, use_live_value, use_now};
use crate::logic::live::{apply_revisions, apply_rollouts};
use crate::logic::timeline::{Show, apply_action_runs, complete_since, starts_action_run};
use crate::store::use_live;

/// The toggle's choice, kept in the URL so a link shows what its sender saw
/// (`?show=actions`).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct ShowQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    show: Option<String>,
}

#[function_component(Rollouts)]
pub fn rollouts() -> Html {
    let now = use_now(30);
    let actions = use_revision_actions();
    let live = use_live();
    let show = Show::parse(
        use_location()
            .and_then(|l| l.query::<ShowQuery>().ok())
            .and_then(|q| q.show)
            .as_deref(),
    );
    let on_show = {
        let navigator = use_navigator();
        Callback::from(move |v: AttrValue| {
            if let Some(n) = &navigator {
                let show = Show::parse(Some(v.as_str())).as_query().map(Into::into);
                let _ = n.replace_with_query(&Route::Rollouts, &ShowQuery { show });
            }
        })
    };

    let revisions = use_fetch((), |_| api::revisions(0));
    let rollouts = use_fetch((), |_| api::rollouts(0));
    let runs = use_fetch((), |_| api::action_runs(None));
    let (revs, revs_more) = use_pages(&revisions, |p, e| apply_revisions(&mut p.items, e));
    let (rolls, rolls_more) = use_pages(&rollouts, |p, e| apply_rollouts(&mut p.items, e));
    let (acts, acts_more) = use_pages(&runs, |p, e| apply_action_runs(&mut p.items, e));

    // An action run started somewhere since the runs were fetched: fetch
    // them again to show it.
    {
        let started = runs.data.as_ref().is_some_and(|d| {
            live.log
                .since(d.0)
                .is_some_and(|events| events.iter().any(|e| starts_action_run(e)))
        });
        let reload = runs.reload.clone();
        use_effect_with(started, move |started| {
            if *started {
                reload.emit(());
            }
            || ()
        });
    }

    // Which lists the view draws on, and so which still have older pages:
    // the timeline stops where the first of those runs out.
    let (commits, acted) = (show != Show::Actions, show != Show::Repo);
    let since = complete_since([
        (
            commits && revs_more.next().is_some(),
            revs.as_ref().and_then(|l| l.last()).map(|r| r.created_at),
        ),
        (
            show == Show::All && rolls_more.next().is_some(),
            rolls.as_ref().and_then(|l| l.last()).map(|r| r.started_at),
        ),
        (
            acted && acts_more.next().is_some(),
            acts.as_ref().and_then(|l| l.last()).map(|r| r.started_at),
        ),
    ]);
    let older = (commits && revs_more.next().is_some())
        || (acted && (rolls_more.next().is_some() || acts_more.next().is_some()));

    // Older pages of every list come together, so an older revision finds
    // its rollout and the timeline stays in order.
    let more_busy = use_state(|| false);
    let load_more = {
        let (revs_more, rolls_more, acts_more, busy) = (
            revs_more.clone(),
            rolls_more.clone(),
            acts_more.clone(),
            more_busy.clone(),
        );
        Callback::from(move |_: MouseEvent| {
            let (revs_more, rolls_more, acts_more, busy) = (
                revs_more.clone(),
                rolls_more.clone(),
                acts_more.clone(),
                busy.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                futures::join!(
                    revs_more
                        .fetch(|o| async move { api::revisions(o.parse().unwrap_or(0)).await }),
                    rolls_more
                        .fetch(|o| async move { api::rollouts(o.parse().unwrap_or(0)).await }),
                    acts_more.fetch(|b| async move { api::action_runs(Some(b)).await }),
                );
                busy.set(false);
            });
        })
    };

    // A server without the action runs listing leaves them out of All.
    let (state, error, retry) = match show {
        Show::Actions => (
            rolls
                .as_ref()
                .zip(acts.as_ref())
                .map(|(r, a)| r.iter().any(|x| x.action.is_some()) || !a.is_empty()),
            rollouts.error.clone().or(runs.error.clone()),
            {
                let (a, b) = (rollouts.reload.clone(), runs.reload.clone());
                Callback::from(move |_| {
                    a.emit(());
                    b.emit(());
                })
            },
        ),
        _ => (
            revs.as_ref().map(|l| !l.is_empty()),
            revisions.error.clone(),
            revisions.reload.clone(),
        ),
    };
    let body = match (state, error) {
        (Some(false), _) if show == Show::Actions => html! {
            <Empty title="No actions yet" message="Actions run from Run action… on the Hosts and Workloads pages, or from a target's page." />
        },
        (Some(false), _) => html! {
            <Empty title="No revisions yet" message="Revisions appear as the server's source watcher sees commits (or a webhook tells it)." />
        },
        (Some(true), _) => html! { <>
            <Timeline revisions={revs.clone().unwrap_or_default()} rollouts={rolls.clone().unwrap_or_default()}
                runs={acts.clone().unwrap_or_default()} {show} {since} {now} open={actions.open.clone()} />
            if older {
                <div class="more"><Button onclick={load_more} busy={*more_busy}>{ "Load older" }</Button></div>
            }
        </> },
        (None, Some(e)) => html! { <ErrorState error={e} {retry} /> },
        (None, None) => html! { <Loading label="Loading rollouts…" /> },
    };

    let subtitle = match show {
        Show::All => {
            "Commits of the configuration repository and operators' actions, newest first, with how each one went."
        }
        Show::Repo => {
            "Commits of the configuration repository, newest first, with how each one's rollout is going."
        }
        Show::Actions => "Actions operators ran, across the fleet or on one target, newest first.",
    };
    let reload_all = {
        let (a, b, c) = (
            revisions.reload.clone(),
            rollouts.reload.clone(),
            runs.reload.clone(),
        );
        Callback::from(move |_| {
            a.emit(());
            b.emit(());
            c.emit(());
        })
    };
    html! {
        <div class="page">
            <PageHead title="Rollouts" subtitle={html! { subtitle }}
                actions={html! { <>
                    <Segmented label="Show" testid="rollouts-show" value={show.as_query().unwrap_or("all")} on_change={on_show}
                        options={vec![("all".into(), "All".into()), ("repo".into(), "Repo".into()), ("actions".into(), "Actions".into())]} />
                    <Refresh loading={revisions.loading || rollouts.loading || runs.loading} reload={reload_all} />
                </> }} />
            <Panel>{ body }</Panel>
            { actions.dialog(&rolls.unwrap_or_default()) }
        </div>
    }
}

/// The older pages of a list, loaded on request.
#[derive(Clone)]
struct More<T: Clone + 'static> {
    extra: UseStateHandle<Vec<T>>,
    next: UseStateHandle<Option<String>>,
}

impl<T: Clone + 'static> More<T> {
    fn next(&self) -> Option<&String> {
        self.next.as_ref()
    }

    /// Appends the page after the last one, if there is one: `get` is
    /// handed the list's cursor (an offset, or the last item's id).
    async fn fetch<F, Fut>(&self, get: F)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<Page<T>, crate::api::ApiError>>,
    {
        let Some(cursor) = (*self.next).clone() else {
            return;
        };
        if let Ok(p) = get(cursor).await {
            let mut all = (*self.extra).clone();
            all.extend(p.items);
            self.extra.set(all);
            self.next.set(p.next);
        }
    }
}

/// A paged list kept live: the first page as fetched, with older pages
/// appended as they are loaded.
#[hook]
fn use_pages<T>(
    page: &Fetch<Page<T>>,
    apply: fn(&mut Page<T>, &LiveEvent),
) -> (Option<Vec<T>>, More<T>)
where
    T: Clone + PartialEq + 'static,
{
    let extra = use_state(Vec::<T>::new);
    let next = use_state(|| None::<String>);
    {
        let (next, extra) = (next.clone(), extra.clone());
        let first_next = page.value().and_then(|p| p.next.clone());
        use_effect_with(first_next, move |n| {
            next.set(n.clone());
            extra.set(Vec::new());
            || ()
        });
    }
    let list = use_live_value(page, apply).map(|mut p| {
        p.items.extend(extra.iter().cloned());
        p.items
    });
    (list, More { extra, next })
}
