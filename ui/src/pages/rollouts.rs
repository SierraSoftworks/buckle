//! Rollouts (design §12): every rollout with its state and revision, newest
//! first, kept live.

use buckle_api::RolloutSummary;
use yew::prelude::*;

use crate::api;
use crate::app::Route;
use crate::components::meter::rollout_segments;
use crate::components::{
    Button, Empty, ErrorState, Loading, Meter, PageHead, Panel, Pill, Refresh, RouteLink,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::format::{
    commit, first_line, relative, rollout_label, rollout_note, short_duration, timestamp,
};
use crate::logic::live::apply_rollouts;
use crate::logic::tone::{rollout_finished, rollout_state_label, rollout_state_tone};

#[function_component(Rollouts)]
pub fn rollouts() -> Html {
    let now = use_now(30);
    let extra = use_state(Vec::<RolloutSummary>::new);
    let next = use_state(|| None::<String>);
    let more_busy = use_state(|| false);
    let page = use_fetch((), |_| api::rollouts(0));
    {
        let (next, extra) = (next.clone(), extra.clone());
        let first_next = page.value().and_then(|p| p.next.clone());
        use_effect_with(first_next, move |n| {
            next.set(n.clone());
            extra.set(Vec::new());
            || ()
        });
    }
    let list = use_fetch_items(&page);
    let list = list.map(|mut l| {
        l.extend(extra.iter().cloned());
        l
    });

    let load_more = {
        let (next, extra, more_busy) = (next.clone(), extra.clone(), more_busy.clone());
        Callback::from(move |_: MouseEvent| {
            let Some(offset) = next.as_ref().and_then(|n| n.parse::<u64>().ok()) else {
                return;
            };
            let (next, extra, more_busy) = (next.clone(), extra.clone(), more_busy.clone());
            more_busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(p) = api::rollouts(offset).await {
                    let mut all = (*extra).clone();
                    all.extend(p.items);
                    extra.set(all);
                    next.set(p.next);
                }
                more_busy.set(false);
            });
        })
    };

    let body = match (&list, &page.error) {
        (Some(l), _) if l.is_empty() => html! {
            <Empty title="No rollouts yet" message="A rollout starts when a valid revision arrives (trigger: auto) or when an operator starts one from Revisions." />
        },
        (Some(l), _) => html! { <>
            <div class="table-wrap">
                <table class="table">
                    <thead><tr>
                        <th scope="col">{ "Rollout" }</th>
                        <th scope="col">{ "State" }</th>
                        <th scope="col">{ "Revision" }</th>
                        <th scope="col">{ "Progress" }</th>
                        <th scope="col">{ "Triggered by" }</th>
                        <th scope="col">{ "Started" }</th>
                        <th scope="col">{ "Took" }</th>
                    </tr></thead>
                    <tbody>
                        { for l.iter().map(|r| row(r, now)) }
                    </tbody>
                </table>
            </div>
            if next.is_some() {
                <div class="more"><Button onclick={load_more} busy={*more_busy}>{ "Load older rollouts" }</Button></div>
            }
        </> },
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={page.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading rollouts…" /> },
    };

    html! {
        <div class="page">
            <PageHead title="Rollouts" subtitle={html! { "Each revision moving through its phases, newest first." }}
                actions={html! { <Refresh loading={page.loading} reload={page.reload.clone()} /> }} />
            <Panel>{ body }</Panel>
        </div>
    }
}

#[hook]
fn use_fetch_items(
    page: &crate::hooks::Fetch<buckle_api::Page<RolloutSummary>>,
) -> Option<Vec<RolloutSummary>> {
    use_live_value(page, |p, e| apply_rollouts(&mut p.items, e)).map(|p| p.items)
}

fn row(r: &RolloutSummary, now: chrono::DateTime<chrono::Utc>) -> Html {
    let took = r
        .finished_at
        .map(|f| short_duration(f.signed_duration_since(r.started_at).num_seconds()))
        .unwrap_or_else(|| "—".into());
    html! {
        <tr class="row-link">
            <td><RouteLink to={Route::rollout(&r.id)} classes="strong row-link__target">{ rollout_label(&r.id) }</RouteLink></td>
            <td>
                <Pill tone={rollout_state_tone(r.state)} label={rollout_state_label(r.state)} title={rollout_note(r)} />
                if let Some(p) = r.current_phase.as_ref().filter(|_| !rollout_finished(r.state)) {
                    <span class="cell-sub">{ format!("in {p}") }</span>
                }
                if let Some(by) = &r.superseded_by {
                    <span class="cell-sub">{ "superseded by " }<RouteLink to={Route::rollout(by)} classes="row-link__also">{ rollout_label(by) }</RouteLink></span>
                }
            </td>
            <td>
                <span class="mono">{ commit(&r.revision) }</span>
                <span class="cell-sub">{ first_line(&r.revision_message).to_string() }</span>
            </td>
            <td>
                <Meter segments={rollout_segments(r)} total={r.hosts_total as usize} />
                <span class="cell-sub">
                    { format!("{}/{} done", r.hosts_done, r.hosts_total) }
                    if r.hosts_failed > 0 { <span class="text-danger">{ format!(" · {} failed", r.hosts_failed) }</span> }
                </span>
            </td>
            <td>{ r.triggered_by.clone() }</td>
            <td title={timestamp(r.started_at)}>{ relative(r.started_at, now) }</td>
            <td>{ took }</td>
        </tr>
    }
}
