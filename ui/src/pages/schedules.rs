//! Schedules: each of the stable revision's `schedules/<name>.yaml` on one
//! line: what it runs and where, when it fires next and how its latest
//! occurrence went. Click a row to open the schedule.

use buckle_api::{ScheduleInfo, ScheduleOccurrence};
use chrono::{DateTime, Utc};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::Route;
use crate::components::timeline::{cron_text, schedule_label};
use crate::components::{Empty, ErrorState, Loading, PageHead, Panel, Pill, Refresh, RouteLink};
use crate::hooks::{use_fetch, use_now};
use crate::logic::format::{count, relative, rollout_label, timestamp};
use crate::logic::tone::{Tone, rollout_state_label, rollout_state_tone};

#[function_component(Schedules)]
pub fn schedules() -> Html {
    let now = use_now(30);
    let navigator = use_navigator();
    let list = use_fetch((), |_| api::schedules());
    let on_open = Callback::from(move |(e, name): (MouseEvent, String)| {
        let own = e
            .target_dyn_into::<web_sys::Element>()
            .and_then(|t| t.closest("a, button").ok().flatten())
            .is_some();
        if let (false, Some(n)) = (own, &navigator) {
            n.push(&Route::Schedule { name });
        }
    });
    let (body, subtitle) = match (list.value(), &list.error) {
        (None, Some(e)) => (
            html! { <ErrorState error={e.clone()} retry={list.reload.clone()} /> },
            None,
        ),
        (None, None) => (html! { <Loading label="Loading schedules…" /> }, None),
        (Some(l), _) if l.is_empty() => (
            html! {
                <Empty title="No schedules"
                    message="Add a schedules/<name>.yaml to the configuration repository: a cron expression, an action and the targets to run it on." />
            },
            None,
        ),
        (Some(l), _) => (
            html! {
                <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true attrs={yewi::attrs! { "data-testid" => "schedules-table" }}>
                    <thead><tr>
                        <th scope="col">{ "Schedule" }</th>
                        <th scope="col">{ "Action" }</th>
                        <th scope="col">{ "Runs on" }</th>
                        <th scope="col">{ "When" }</th>
                        <th scope="col">{ "Next" }</th>
                        <th scope="col">{ "Latest" }</th>
                    </tr></thead>
                    <tbody>{ for l.iter().map(|s| row(s, &on_open, now)) }</tbody>
                </yewi::SimpleTable>
            },
            Some(count(l.len(), "schedule", "schedules")),
        ),
    };
    html! {
        <div class="page page--wide">
            <PageHead title="Schedules"
                subtitle={html! { { subtitle.map_or_else(|| "Actions the stable revision rolls out on a schedule.".to_string(), |c| format!("{c} in the stable revision, each rolling its action out as it falls due.")) } }}
                actions={html! { <Refresh loading={list.loading} reload={list.reload.clone()} /> }} />
            <Panel>{ body }</Panel>
        </div>
    }
}

/// Where a schedule runs its action: the selector, or every target
/// offering it.
pub fn scope(s: &ScheduleInfo) -> Html {
    let noun = if s.workload { "placement" } else { "target" };
    match &s.selector {
        Some(sel) => html! { <span class="mono">{ sel.clone() }</span> },
        None => html! { <span class="muted">{ format!("every {noun} offering it") }</span> },
    }
}

/// How an occurrence went: its rollout (opening it) with its state unless
/// `progress` follows, or skipped or waiting, with why.
pub fn outcome(o: &ScheduleOccurrence, now: DateTime<Utc>, progress: bool) -> Html {
    let at = html! { <span class="muted" title={timestamp(o.due_at)}>{ relative(o.due_at, now) }</span> };
    if let Some(r) = &o.rollout {
        return html! { <span class="schedule-outcome">
            <RouteLink to={Route::rollout(&r.id)} classes="schedule-outcome__rollout" title={format!("Open rollout {}", rollout_label(&r.id))}>
                { rollout_label(&r.id) }
            </RouteLink>
            if !progress {
                <Pill tone={rollout_state_tone(r.state)} label={rollout_state_label(r.state)} />
            }
            if !progress && r.hosts_failed > 0 {
                <span class="text-danger">{ format!("{} failed", r.hosts_failed) }</span>
            }
            { at }
        </span> };
    }
    if let Some(why) = &o.waiting {
        return html! { <span class="schedule-outcome">
            <Pill tone={Tone::Info} label="waiting" title={Some(AttrValue::from(why.clone()))} />{ at }
        </span> };
    }
    let why = o.skipped.clone().unwrap_or_default();
    html! { <span class="schedule-outcome">
        <Pill tone={Tone::Neutral} label="skipped" title={Some(AttrValue::from(why.clone()))} />{ at }
        <span class="cell-sub">{ why }</span>
    </span> }
}

fn row(s: &ScheduleInfo, on_open: &Callback<(MouseEvent, String)>, now: DateTime<Utc>) -> Html {
    let onclick = {
        let (on_open, name) = (on_open.clone(), s.name.clone());
        Callback::from(move |e: MouseEvent| on_open.emit((e, name.clone())))
    };
    html! {
        <tr data-schedule={s.name.clone()} class="yi-table__row yi-table__row--clickable" {onclick}>
            <td>
                <RouteLink to={Route::Schedule { name: s.name.clone() }} classes="schedule-name">{ s.name.clone() }</RouteLink>
                if let Some(d) = &s.description { <span class="cell-sub schedule-description">{ d.clone() }</span> }
            </td>
            <td class="nowrap">
                { schedule_label(s) }
                <span class="cell-sub mono">{ format!("{}/{}", s.module, s.action) }</span>
            </td>
            <td>{ scope(s) }</td>
            <td class="nowrap">
                { cron_text(s) }
                <span class="cell-sub">
                    { s.timezone.clone() }
                    if s.approval == "auto" { { " · unattended" } }
                </span>
            </td>
            <td class="nowrap">
                { match s.next_at {
                    Some(at) => html! { <span title={timestamp(at)}>{ relative(at, now) }</span> },
                    None => html! { <span class="muted">{ "never" }</span> },
                } }
            </td>
            <td>
                { match &s.last {
                    Some(o) => outcome(o, now, false),
                    None => html! { <span class="muted">{ "not yet" }</span> },
                } }
            </td>
        </tr>
    }
}
