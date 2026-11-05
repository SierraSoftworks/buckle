//! A workload's page: what it is (its needs, the workloads that need it,
//! its resources and removal policy), where the active rollout has it, and
//! one card per cluster it runs on, the cluster page's Workloads row for
//! it opened (checks, the run's steps, resources linked into the
//! scheduler's UI) with that placement's actions. `?cluster=` scrolls to
//! that cluster's card.

use std::collections::BTreeMap;

use buckle_api::{Connection, HostDetail, HostRolloutState, HostStatus, HostSummary};
use yew::prelude::*;
use yew_router::prelude::*;

use super::modules_card::ModulesPanel;
use super::workloads::{cluster_details, placement_chip};
use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Empty, ErrorState, Loading, PageHead, Panel, Pill, Refresh, RouteLink, use_target_actions,
};
use crate::hooks::{use_fetch, use_live_value};
use crate::logic::format::{count, rollout_label};
use crate::logic::live::{apply_fleet, apply_rollout};
use crate::logic::tone::{Tone, host_state_label, host_state_tone};
use crate::logic::workloads::gather;

#[derive(Properties, PartialEq)]
pub struct Props {
    pub name: String,
}

#[derive(Clone, Default, PartialEq, serde::Deserialize)]
struct Query {
    #[serde(default)]
    cluster: Option<String>,
}

#[function_component(WorkloadPage)]
pub fn workload_page(props: &Props) -> Html {
    let auth = use_auth();
    let location = use_location();
    let focus = location
        .and_then(|l| l.query::<Query>().ok())
        .and_then(|q| q.cluster);
    let data = use_fetch((), |_| cluster_details());
    let fleet = use_fetch((), |_| api::fleet());
    let fleet = use_live_value(&fleet, apply_fleet);
    let active = fleet.as_ref().and_then(|f| f.active_rollout.clone());
    let rollout = use_fetch(active.clone(), |id| async move {
        match id {
            Some(id) => api::rollout(&id).await.map(Some),
            None => Ok(None),
        }
    });
    let rollout = use_live_value(&rollout, |d: &mut Option<buckle_api::RolloutDetail>, e| {
        if let Some(d) = d {
            apply_rollout(d, e)
        }
    })
    .flatten();
    // Opened from a cluster, a pipeline block or a Targets row: land on
    // that cluster's card once the cards are drawn.
    {
        let loaded = data.value().is_some();
        use_effect_with((focus.clone(), loaded), move |(focus, loaded)| {
            if let (Some(c), true) = (focus, loaded)
                && let Some(el) = gloo_utils_document().get_element_by_id(&format!("on-{c}"))
            {
                el.scroll_into_view();
            }
            || ()
        });
    }

    let Some(details) = data.value() else {
        return html! {
            <div class="page">
                <PageHead title={props.name.clone()} />
                { match &data.error {
                    Some(e) => html! { <ErrorState error={e.clone()} retry={data.reload.clone()} /> },
                    None => html! { <Loading label="Loading the workload…" /> },
                } }
            </div>
        };
    };
    let pairs: Vec<_> = details.iter().map(|d| (d.clone(), None)).collect();
    let all = gather(&pairs);
    let Some(w) = all.iter().find(|w| w.name == props.name) else {
        return html! {
            <div class="page">
                <PageHead title={props.name.clone()} />
                <Empty title="No such workload" message="No cluster runs a workload of this name, and no revision selects one for a cluster." />
            </div>
        };
    };
    let hosts: BTreeMap<String, HostSummary> = fleet
        .as_ref()
        .map(|f| f.hosts.iter().map(|h| (h.id.clone(), h.clone())).collect())
        .unwrap_or_default();
    let views: Vec<_> = details
        .iter()
        .filter_map(|d| {
            d.workloads
                .iter()
                .find(|v| v.name == w.name)
                .map(|v| (d.summary.id.as_str(), v))
        })
        .collect();
    let needed_by = w.needed_by(&all);
    let link = |n: &str| match n.strip_prefix("module:") {
        Some(m) => {
            html! { <span class="badge mono" title="a module on the cluster's nodes">{ m.to_owned() }</span> }
        }
        None => {
            html! { <RouteLink to={Route::Workload { name: n.to_owned() }} classes={classes!("mono")}>{ n.to_owned() }</RouteLink> }
        }
    };
    let in_rollout: Vec<_> = rollout
        .as_ref()
        .map(|d| {
            d.phases
                .iter()
                .flat_map(|p| &p.hosts)
                .filter(|h| h.workload.as_deref() == Some(w.name.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    html! {
        <div class="page page--wide">
            <PageHead title={w.name.clone()}
                subtitle={html! { { if w.description.is_empty() { format!("A workload on {}", count(w.placements.len(), "cluster", "clusters")) } else { w.description.clone() } } }}
                actions={html! { <Refresh loading={data.loading} reload={data.reload.clone()} /> }} />
            <Panel title="Summary">
                <dl class="workload-summary">
                    <dt>{ "Runs on" }</dt>
                    <dd><span class="chips">{ for w.placements.iter().map(|p| placement_chip(&w.name, &p.cluster, p.row.state)) }</span></dd>
                    <dt>{ "Needs" }</dt>
                    <dd>if w.needs.is_empty() { <span class="muted">{ "nothing" }</span> } else { <span class="chips">{ for w.needs.iter().map(|n| link(n)) }</span> }</dd>
                    <dt>{ "Needed by" }</dt>
                    <dd>if needed_by.is_empty() { <span class="muted">{ "nothing" }</span> } else { <span class="chips">{ for needed_by.iter().map(|n| link(n)) }</span> }</dd>
                    if let Some((_, v)) = views.first() {
                        <dt>{ "Resources" }</dt>
                        <dd class="mono">{ if v.resources.is_empty() { "—".to_owned() } else { v.resources.join(" → ") } }</dd>
                        <dt>{ "On removal" }</dt>
                        <dd>{ v.removal.clone() }</dd>
                    }
                    if !in_rollout.is_empty() && let Some(d) = rollout.as_ref() {
                        <dt>{ "Rollout" }</dt>
                        <dd data-testid="workload-rollout">
                            <RouteLink to={Route::rollout(&d.summary.id)}>{ rollout_label(&d.summary.id) }</RouteLink>
                            { for in_rollout.iter().map(|h| {
                                let quiet = crate::logic::units::unchanged(h) && h.state != HostRolloutState::Failed;
                                html! {
                                    <span class="workload-rollout__unit" data-host={h.host.clone()}>
                                        { format!(" · {}: ", h.host) }
                                        if quiet { <Pill tone={Tone::Neutral} label="unchanged" /> }
                                        else { <Pill tone={host_state_tone(h.state)} label={host_state_label(h.state)} /> }
                                        <span class="muted">{ format!(" in {}", h.phase) }</span>
                                    </span>
                                }
                            }) }
                        </dd>
                    }
                </dl>
            </Panel>
            { for details.iter().filter(|d| w.placements.iter().any(|p| p.cluster == d.summary.id)).map(|d| html! {
                <PlacementCard key={d.summary.id.clone()} detail={d.clone()} workload={w.name.clone()}
                    admin={auth.is_admin()} hosts={hosts.clone()} reload={data.reload.clone()} />
            }) }
        </div>
    }
}

fn gloo_utils_document() -> web_sys::Document {
    web_sys::window()
        .and_then(|w| w.document())
        .expect("a document")
}

#[derive(Properties, PartialEq)]
struct CardProps {
    detail: HostDetail,
    workload: String,
    admin: bool,
    hosts: BTreeMap<String, HostSummary>,
    reload: Callback<()>,
}

/// The workload on one cluster: the cluster page's row for it, opened,
/// with that placement's actions.
#[function_component(PlacementCard)]
fn placement_card(props: &CardProps) -> Html {
    let d = &props.detail;
    let picked = use_state_eq(|| None::<String>);
    let actions = {
        let (picked, reload) = (picked.clone(), props.reload.clone());
        use_target_actions(
            d.summary.id.clone(),
            Callback::from(move |_| {
                picked.set(None);
                reload.emit(());
            }),
        )
    };
    let status = d.summary.cluster_status.clone().unwrap_or_default();
    let can_act = props.admin
        && d.summary.status == HostStatus::Active
        && status
            .executor
            .as_deref()
            .and_then(|e| props.hosts.get(e))
            .is_some_and(|h| h.connection == Connection::Online);
    html! { <>
        <ModulesPanel detail={d.clone()} cluster=true only={props.workload.clone()}
            on_action={can_act.then(|| actions.on_action.clone())}
            newest={d.latest_run.as_ref().map(|r| r.id.clone())} picked={(*picked).clone()}
            on_pick={{ let picked = picked.clone(); Callback::from(move |run| picked.set(run)) }} />
        { actions.dialog }
    </> }
}
