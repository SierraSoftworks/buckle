//! A schedule's page: what it runs, where and when; the targets it picks
//! now, each with how the latest run went there; and every occurrence,
//! newest first, with the rollout it started (or why it started none).

use buckle_api::{ScheduleDetail, ScheduleOccurrence, ScheduleTarget, TargetKind};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::schedules::{outcome, scope};
use crate::api;
use crate::app::Route;
use crate::components::revision::RolloutProgress;
use crate::components::timeline::{cron_text, schedule_label};
use crate::components::ui::facts_list;
use crate::components::{
    Empty, ErrorState, HostLink, Loading, PageHead, Panel, Pill, Refresh, RouteLink,
};
use crate::hooks::{use_fetch, use_now};
use crate::logic::format::{relative, short_duration, timestamp};
use crate::logic::tone::{host_state_label, host_state_tone};

#[derive(Properties, PartialEq)]
pub struct Props {
    pub name: String,
}

#[function_component(SchedulePage)]
pub fn schedule_page(props: &Props) -> Html {
    let now = use_now(30);
    let data = use_fetch(props.name.clone(), |name| async move {
        api::schedule(&name).await
    });
    let refresh = html! { <Refresh loading={data.loading} reload={data.reload.clone()} /> };
    let Some(d) = data.value() else {
        return html! {
            <div class="page">
                <PageHead title={props.name.clone()} actions={refresh} />
                { match &data.error {
                    Some(crate::api::ApiError::NotFound(_)) => html! {
                        <Empty title="No such schedule" message="The stable revision has no schedules/ file of this name." />
                    },
                    Some(e) => html! { <ErrorState error={e.clone()} retry={data.reload.clone()} /> },
                    None => html! { <Loading label="Loading the schedule…" /> },
                } }
            </div>
        };
    };
    let s = &d.schedule;
    let subtitle = s.description.clone().unwrap_or_else(|| {
        format!(
            "Rolls {} out {}.",
            schedule_label(s),
            cron_text(s).to_lowercase()
        )
    });
    html! {
        <div class="page page--wide schedule-page">
            <PageHead title={s.name.clone()} subtitle={html! { subtitle }} actions={refresh} />
            <Panel title="Schedule">{ summary(d, now) }</Panel>
            <Panel title="Targets" subtitle="Those it picks now, with how its latest run went on each.">
                { targets(&d.targets, now) }
            </Panel>
            <Panel title="Runs" subtitle="Each time it fell due, newest first.">
                { history(&d.history, now) }
            </Panel>
        </div>
    }
}

fn summary(d: &ScheduleDetail, now: DateTime<Utc>) -> Html {
    let s = &d.schedule;
    let noun = if s.workload { "Workload" } else { "Module" };
    facts_list(vec![
        (
            "Action",
            html! { <>
                { schedule_label(s) }
                <span class="muted mono">{ format!(" {}/{}", s.module, s.action) }</span>
                <span class="muted">{ format!(" ({} action)", noun.to_lowercase()) }</span>
            </> },
        ),
        ("Runs on", scope(s)),
        (
            "When",
            html! { <>
                { cron_text(s) }
                <span class="muted">{ format!(" ({}) · ", s.timezone) }</span>
                <span class="mono muted">{ s.cron.clone() }</span>
            </> },
        ),
        (
            "Next",
            match s.next_at {
                Some(at) => {
                    html! { <span title={timestamp(at)}>{ format!("{} · {}", relative(at, now), timestamp(at)) }</span> }
                }
                None => html! { <span class="muted">{ "never again" }</span> },
            },
        ),
        (
            "Approval",
            if s.approval == "auto" {
                html! { <span>{ "Unattended: its phases go ahead without approvals." }</span> }
            } else {
                html! { <span>{ "As the phases say: one that asks for approval waits for an operator." }</span> }
            },
        ),
        (
            "Window",
            html! { <span>{ format!("Waits up to {} for a rollout under way, then skips that run.", short_duration(s.window_seconds as i64)) }</span> },
        ),
    ])
}

fn targets(list: &[ScheduleTarget], now: DateTime<Utc>) -> Html {
    if list.is_empty() {
        return html! {
            <Empty title="No targets" message="No active target offers the action that the selector matches." />
        };
    }
    html! {
        <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true attrs={yewi::attrs! { "data-testid" => "schedule-targets" }}>
            <thead><tr>
                <th scope="col">{ "Target" }</th>
                <th scope="col">{ "Latest run" }</th>
                <th scope="col">{ "Phase" }</th>
                <th scope="col">{ "Updated" }</th>
            </tr></thead>
            <tbody>{ for list.iter().map(|t| html! {
                <tr data-target={t.id.clone()}>
                    <td class="nowrap">
                        if t.kind == TargetKind::Host {
                            <HostLink id={t.id.clone()} {now} />
                        } else {
                            <RouteLink to={Route::target(&t.id, t.kind)} classes="mono strong">{ t.id.clone() }</RouteLink>
                        }
                    </td>
                    <td>
                        { match &t.last {
                            Some(r) => html! { <>
                                <Pill tone={host_state_tone(r.state)} label={host_state_label(r.state)} />
                                if let Some(e) = &r.error { <span class="cell-sub text-danger">{ e.clone() }</span> }
                            </> },
                            None => html! { <span class="muted">{ "not in its latest run" }</span> },
                        } }
                    </td>
                    <td>{ t.last.as_ref().map(|r| r.phase.clone()).unwrap_or_default() }</td>
                    <td class="nowrap">
                        { t.last.as_ref().map(|r| html! { <span title={timestamp(r.updated_at)}>{ relative(r.updated_at, now) }</span> }).unwrap_or_default() }
                    </td>
                </tr>
            }) }</tbody>
        </yewi::SimpleTable>
    }
}

fn history(list: &[ScheduleOccurrence], now: DateTime<Utc>) -> Html {
    if list.is_empty() {
        return html! { <Empty title="No runs yet" message="It has not fallen due since the server first saw it." /> };
    }
    html! {
        <ol class="schedule-runs" data-testid="schedule-runs">
            { for list.iter().map(|o| html! {
                <li class="schedule-runs__item">
                    <div class="schedule-runs__head">
                        { outcome(o, now, true) }
                        <span class="muted">{ timestamp(o.due_at) }</span>
                    </div>
                    if let Some(r) = &o.rollout {
                        <RolloutProgress rollout={r.clone()} {now} />
                    }
                </li>
            }) }
        </ol>
    }
}
