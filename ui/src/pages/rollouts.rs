//! Rollouts (design §12): the configuration repository's commits, newest
//! first, each with how its rollout is going and the one control that fits
//! where it stands: [Stable], Rollback, Review or [Running]. Revisions and
//! rollouts are one history, so there is one page for both.

use buckle_api::{LiveEvent, Page};
use yew::prelude::*;

use crate::api;
use crate::components::{
    Button, Empty, ErrorState, Loading, PageHead, Panel, Refresh, RevisionLog, use_revision_actions,
};
use crate::hooks::{Fetch, use_fetch, use_live_value, use_now};
use crate::logic::live::{apply_revisions, apply_rollouts};

#[function_component(Rollouts)]
pub fn rollouts() -> Html {
    let now = use_now(30);
    let actions = use_revision_actions();

    let revisions = use_fetch((), |_| api::revisions(0));
    let rollouts = use_fetch((), |_| api::rollouts(0));
    let (revs, revs_more) = use_pages(&revisions, |p, e| apply_revisions(&mut p.items, e));
    let (rolls, rolls_more) = use_pages(&rollouts, |p, e| apply_rollouts(&mut p.items, e));

    // Older pages of both lists come together, so an older revision finds
    // its rollout.
    let more_busy = use_state(|| false);
    let load_more = {
        let (revs_more, rolls_more, busy) =
            (revs_more.clone(), rolls_more.clone(), more_busy.clone());
        Callback::from(move |_: MouseEvent| {
            let (revs_more, rolls_more, busy) =
                (revs_more.clone(), rolls_more.clone(), busy.clone());
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                futures::join!(
                    revs_more.fetch(|o| async move { api::revisions(o).await }),
                    rolls_more.fetch(|o| async move { api::rollouts(o).await }),
                );
                busy.set(false);
            });
        })
    };

    let body = match (&revs, &revisions.error) {
        (Some(l), _) if l.is_empty() => html! {
            <Empty title="No revisions yet" message="Revisions appear as the server's source watcher sees commits (or a webhook tells it)." />
        },
        (Some(l), _) => html! { <>
            <RevisionLog revisions={l.clone()} rollouts={rolls.clone().unwrap_or_default()} {now} open={actions.open.clone()} />
            if revs_more.next().is_some() {
                <div class="more"><Button onclick={load_more} busy={*more_busy}>{ "Load older revisions" }</Button></div>
            }
        </> },
        (None, Some(e)) => {
            html! { <ErrorState error={e.clone()} retry={revisions.reload.clone()} /> }
        }
        (None, None) => html! { <Loading label="Loading rollouts…" /> },
    };

    html! {
        <div class="page">
            <PageHead title="Rollouts" subtitle={html! { "Commits of the configuration repository, newest first, with how each one's rollout is going." }}
                actions={html! { <Refresh loading={revisions.loading || rollouts.loading} reload={{
                    let (a, b) = (revisions.reload.clone(), rollouts.reload.clone());
                    Callback::from(move |_| { a.emit(()); b.emit(()); })
                }} /> }} />
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

    /// Appends the page after the last one, if there is one.
    async fn fetch<F, Fut>(&self, get: F)
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = Result<Page<T>, crate::api::ApiError>>,
    {
        let Some(offset) = self.next.as_ref().and_then(|n| n.parse::<u64>().ok()) else {
            return;
        };
        if let Ok(p) = get(offset).await {
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
