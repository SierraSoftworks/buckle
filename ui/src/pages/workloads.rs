//! Workloads: every workload the fleet runs, across all its clusters, one
//! row each. The cluster page's Workloads card turned sideways: how its
//! placements read (a meter and a chip per cluster), the revision applied,
//! drift and when it was last read. Click a row for the workload's page.

use buckle_api::HostDetail;
use serde::{Deserialize, Serialize};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api::{self, ApiError};
use crate::app::{Route, use_auth};
use crate::components::{
    Empty, ErrorState, Loading, Meter, PageHead, Panel, Pill, Refresh, RouteLink, Segment,
    Segmented, WorkloadActions, use_workload_rollout,
};
use crate::hooks::{use_fetch, use_now};
use crate::logic::clusters::WorkloadState;
use crate::logic::format::{commit, count, relative_opt, timestamp};
use crate::logic::tone::Tone;
use crate::logic::workloads::{FleetWorkload, gather, matches};

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    q: String,
    /// `attention`: only workloads failing or drifted somewhere.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    show: String,
}

/// Every cluster's detail, for the workloads they run.
pub(crate) async fn cluster_details() -> Result<Vec<HostDetail>, ApiError> {
    let fleet = api::fleet().await?;
    let details = futures::future::join_all(fleet.clusters.iter().map(|c| api::host(&c.id))).await;
    details.into_iter().collect()
}

pub(crate) fn state_tone(s: WorkloadState) -> Tone {
    match s {
        WorkloadState::Failing => Tone::Danger,
        WorkloadState::Deploying => Tone::Active,
        WorkloadState::Running => Tone::Ok,
        WorkloadState::NotApplied => Tone::Neutral,
    }
}

fn attention(w: &FleetWorkload) -> bool {
    w.state() == WorkloadState::Failing || w.drifted()
}

#[function_component(Workloads)]
pub fn workloads() -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let rollouts = use_workload_rollout();
    // A row's actions roll out across its placements.
    let on_action = auth.is_admin().then(|| rollouts.on_action.clone());
    let location = use_location();
    let navigator = use_navigator();
    let query: Query = location
        .and_then(|l| l.query::<Query>().ok())
        .unwrap_or_default();
    let data = use_fetch((), |_| cluster_details());

    let set_query = {
        let navigator = navigator.clone();
        Callback::from(move |q: Query| {
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(&Route::Workloads, &q);
            }
        })
    };
    let on_show = {
        let (set, q) = (set_query.clone(), query.clone());
        Callback::from(move |v: AttrValue| {
            set.emit(Query {
                show: if v == "all" {
                    String::new()
                } else {
                    v.to_string()
                },
                ..q.clone()
            })
        })
    };
    let on_open = {
        let navigator = navigator.clone();
        Callback::from(move |(e, name): (MouseEvent, String)| {
            let own = e
                .target_dyn_into::<web_sys::Element>()
                .and_then(|t| t.closest("a, button").ok().flatten())
                .is_some();
            if let (false, Some(n)) = (own, &navigator) {
                n.push(&Route::Workload { name });
            }
        })
    };

    let (body, subtitle) = match (data.value(), &data.error) {
        (None, Some(e)) => (
            html! { <ErrorState error={e.clone()} retry={data.reload.clone()} /> },
            String::new(),
        ),
        (None, None) => (
            html! { <Loading label="Loading workloads…" /> },
            String::new(),
        ),
        (Some(details), _) => {
            let pairs: Vec<_> = details.iter().map(|d| (d.clone(), None)).collect();
            let all = gather(&pairs);
            let failing = all
                .iter()
                .filter(|w| w.state() == WorkloadState::Failing)
                .count();
            let drifted = all.iter().filter(|w| w.drifted()).count();
            let mut subtitle = count(all.len(), "workload", "workloads");
            if failing > 0 {
                subtitle.push_str(&format!(" · {failing} failing"));
            }
            if drifted > 0 {
                subtitle.push_str(&format!(" · {drifted} drifted"));
            }
            let rows: Vec<&FleetWorkload> = all
                .iter()
                .filter(|w| matches(w, &query.q))
                .filter(|w| query.show != "attention" || attention(w))
                .collect();
            let body = html! { <>
                <div class="toolbar">
                    <Segmented label="Workloads shown" testid="workloads-view"
                        value={if query.show == "attention" { "attention" } else { "all" }}
                        options={vec![("all".into(), "All".into()), ("attention".into(), "Needs attention".into())]}
                        on_change={on_show} />
                </div>
                if all.is_empty() {
                    <Empty title="No workloads" message="Workloads appear once a cluster's revision selects them: add workloads/NAME/workload.yaml." />
                } else if rows.is_empty() {
                    <Empty title="No workloads match" message="Clear the search or show all." />
                } else {
                    <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true attrs={yewi::attrs! { "data-testid" => "workloads-table" }}>
                            <thead><tr>
                                <th scope="col">{ "Workload" }</th>
                                <th scope="col">{ "Health" }</th>
                                <th scope="col">{ "Clusters" }</th>
                                <th scope="col">{ "Revision" }</th>
                                <th scope="col">{ "Health read" }</th>
                                if on_action.is_some() { <th scope="col"><span class="visually-hidden">{ "Actions" }</span></th> }
                            </tr></thead>
                            <tbody>{ for rows.into_iter().map(|w| row(w, &on_open, on_action.as_ref(), now)) }</tbody>
                    </yewi::SimpleTable>
                }
            </> };
            (body, subtitle)
        }
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Workloads"
                subtitle={html! { { if subtitle.is_empty() { "What the fleet's clusters run.".to_string() } else { subtitle } } }}
                actions={html! { <Refresh loading={data.loading} reload={data.reload.clone()} /> }} />
            <Panel>{ body }</Panel>
            { rollouts.dialog }
        </div>
    }
}

/// A placement's chip: its cluster, coloured by how it reads there.
pub(crate) fn placement_chip(name: &str, cluster: &str, state: WorkloadState) -> Html {
    html! {
        <yewi::Tag class="placement-chip"
            href={yewi::NavTarget::route(Route::Workload { name: name.to_owned() }).query([("cluster", cluster.to_owned())])}
            attrs={yewi::attrs! {
                "title" => format!("{name} on {cluster}: {}", state.label()),
                "data-host" => cluster.to_owned(),
                "data-state" => state.label().replace(' ', "-"),
            }}>
            <yewi::StatusDot tone={state_tone(state).yi()} />
            { cluster.to_owned() }
        </yewi::Tag>
    }
}

fn row(
    w: &FleetWorkload,
    on_open: &Callback<(MouseEvent, String)>,
    on_action: Option<&Callback<(String, buckle_api::ActionInfo)>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let onclick = {
        let (on_open, name) = (on_open.clone(), w.name.clone());
        Callback::from(move |e: MouseEvent| on_open.emit((e, name.clone())))
    };
    let tally = |s: WorkloadState| w.placements.iter().filter(|p| p.row.state == s).count();
    let segments = vec![
        Segment::new(tally(WorkloadState::Running), Tone::Ok, "running"),
        Segment::new(tally(WorkloadState::Deploying), Tone::Active, "deploying"),
        Segment::new(tally(WorkloadState::Failing), Tone::Danger, "failing"),
    ];
    let state = w.state();
    let reasons: Vec<String> = w
        .placements
        .iter()
        .filter_map(|p| p.row.reason.as_ref().map(|r| format!("{}: {r}", p.cluster)))
        .collect();
    html! {
        <tr data-workload={w.name.clone()} data-state={state.label().replace(' ', "-")} class="yi-table__row yi-table__row--clickable" {onclick}>
            <td>
                <RouteLink to={Route::Workload { name: w.name.clone() }} classes={classes!("mono", "strong")}>{ w.name.clone() }</RouteLink>
                if !w.description.is_empty() { <span class="cell-sub">{ w.description.clone() }</span> }
            </td>
            <td class="workloads__health">
                <Meter {segments} total={w.placements.len()} rest="not applied" />
                if state == WorkloadState::Failing {
                    { for reasons.first().map(|r| html! { <span class="cell-sub muted">{ r.clone() }</span> }) }
                }
            </td>
            <td>
                <span class="chips">
                    { for w.placements.iter().map(|p| placement_chip(&w.name, &p.cluster, p.row.state)) }
                </span>
            </td>
            <td class="nowrap">
                { match w.applied() {
                    Some((at, rev)) => html! { <span class="mono" title={format!("{rev}, applied {}", timestamp(*at))}>{ commit(rev) }</span> },
                    None => html! { <span class="muted">{ "nothing yet" }</span> },
                } }
                if w.drifted() { { " " }<Pill tone={Tone::Warn} label="drifted" /> }
            </td>
            <td class="nowrap" title={w.read_at().map(timestamp)}>{ relative_opt(w.read_at(), now) }</td>
            if let Some(on_action) = on_action {
                <td class="row-controls">
                    <WorkloadActions workload={w.name.clone()} actions={w.actions.clone()} on_action={on_action.clone()}
                        small=true testid={format!("workload-actions-{}", w.name)} />
                </td>
            }
        </tr>
    }
}
