//! Clusters: every cluster target on one line, beside (not among) the
//! hosts: its state, its workloads (a count, then only what fails or is
//! deploying), the host speaking for it, its groups, applied revision and
//! last health reading. Hover a name for the cluster's card, click a row
//! to open it. Admins add clusters `buckle.yaml` does not declare.

use std::collections::BTreeMap;

use buckle_api::{ClusterStatus, HealthSummary, HostStatus, HostSummary};
use serde::{Deserialize, Serialize};
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Empty, ErrorState, HostLink, Kind, Loading, MenuItem, PageHead, Panel, Pill, Refresh,
    RunAction, SplitButton, icons,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::clusters::deploying_text;
use crate::logic::format::{commit, count, relative_opt, timestamp};
use crate::logic::live::apply_fleet;
use crate::logic::tone::{Tone, health_label, health_tone, host_status_label, host_status_tone};

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    q: String,
}

fn search_matches(c: &HostSummary, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    let spec = c.cluster.as_ref().map(|i| &i.spec);
    q.is_empty()
        || c.id.to_lowercase().contains(&q)
        || c.groups.iter().any(|g| g.to_lowercase().contains(&q))
        || c.effective_labels
            .iter()
            .any(|(k, v)| format!("{k}={v}").to_lowercase().contains(&q))
        || spec.is_some_and(|s| s.kind.as_str().contains(&q))
}

/// Whether a cluster wants a look: a failing check, or no host to speak
/// for the workloads it has.
fn needs_attention(c: &HostSummary) -> bool {
    let status = c.cluster_status.clone().unwrap_or_default();
    c.health == HealthSummary::Failing
        || !status.failing.is_empty()
        || (status.workloads > 0 && status.executor.is_none())
}

#[function_component(Clusters)]
pub fn clusters() -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let location = use_location();
    let navigator = use_navigator();
    let query: Query = location
        .and_then(|l| l.query::<Query>().ok())
        .unwrap_or_default();
    let fleet = use_fetch((), |_| api::fleet());
    let live = use_live_value(&fleet, apply_fleet);
    let adding = use_state(|| false);
    let running = use_state(|| false);

    let on_search = {
        let navigator = navigator.clone();
        Callback::from(move |e: InputEvent| {
            let q = e.target_unchecked_into::<HtmlInputElement>().value();
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(&Route::Clusters, &Query { q });
            }
        })
    };
    let on_open = {
        let navigator = navigator.clone();
        Callback::from(move |(e, id): (MouseEvent, String)| {
            let own = e
                .target_dyn_into::<web_sys::Element>()
                .and_then(|t| t.closest("a, button, input").ok().flatten())
                .is_some();
            if let (false, Some(n)) = (own, &navigator) {
                n.push(&Route::Cluster { id });
            }
        })
    };
    let add = {
        let adding = adding.clone();
        Callback::from(move |_: MouseEvent| adding.set(true))
    };

    let (body, subtitle) = match (&live, &fleet.error) {
        (None, Some(e)) => (
            html! { <ErrorState error={e.clone()} retry={fleet.reload.clone()} /> },
            String::new(),
        ),
        (None, None) => (
            html! { <Loading label="Loading clusters…" /> },
            String::new(),
        ),
        (Some(f), _) if f.clusters.is_empty() => (
            html! {
                <Empty title="No clusters"
                    message="Declare one under fleet.clusters in buckle.yaml, or add a Nomad cluster the repository does not declare.">
                    if auth.is_admin() {
                        <Button kind={Kind::Primary} onclick={add.clone()}>{ "Add a cluster…" }</Button>
                    }
                </Empty>
            },
            String::new(),
        ),
        (Some(f), _) => {
            let hosts: BTreeMap<&str, &HostSummary> =
                f.hosts.iter().map(|h| (h.id.as_str(), h)).collect();
            let workloads: u32 = f
                .clusters
                .iter()
                .filter_map(|c| c.cluster_status.as_ref())
                .map(|s| s.workloads)
                .sum();
            let attention = f.clusters.iter().filter(|c| needs_attention(c)).count();
            let mut subtitle = format!(
                "{} · {}",
                count(f.clusters.len(), "cluster", "clusters"),
                count(workloads as usize, "workload", "workloads")
            );
            if attention > 0 {
                subtitle.push_str(&format!(
                    " · {attention} {} attention",
                    if attention == 1 { "needs" } else { "need" }
                ));
            }
            let mut rows: Vec<&HostSummary> = f
                .clusters
                .iter()
                .filter(|c| search_matches(c, &query.q))
                .collect();
            rows.sort_by(|a, b| (!needs_attention(a), &a.id).cmp(&(!needs_attention(b), &b.id)));
            let stable = f.stable_revision.as_deref();
            let body = html! { <>
                <div class="toolbar">
                    <input class="input toolbar__search" type="search" placeholder="Search clusters, labels, groups"
                        aria-label="Search clusters" value={query.q.clone()} oninput={on_search} />
                </div>
                if rows.is_empty() {
                    <Empty title="No clusters match" message="Clear the search." />
                } else {
                    <div class="table-wrap">
                        <table class="table" data-testid="clusters-table">
                            <thead><tr>
                                <th scope="col">{ "Cluster" }</th>
                                <th scope="col">{ "State" }</th>
                                <th scope="col">{ "Workloads" }</th>
                                <th scope="col">{ "Steward" }</th>
                                <th scope="col">{ "Groups" }</th>
                                <th scope="col">{ "Revision" }</th>
                                <th scope="col">{ "Health read" }</th>
                            </tr></thead>
                            <tbody>{ for rows.into_iter().map(|c| row(c, &hosts, stable, &on_open, now)) }</tbody>
                        </table>
                    </div>
                }
            </> };
            (body, subtitle)
        }
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Clusters"
                subtitle={html! { { if subtitle.is_empty() { "Nomad and Kubernetes clusters Buckle applies workloads to.".to_string() } else { subtitle } } }}
                actions={html! { <>
                    <Refresh loading={fleet.loading} reload={fleet.reload.clone()} />
                    if auth.is_admin() {
                        <SplitButton menu_label="More cluster actions" testid="clusters-actions" items={vec![
                            MenuItem::new("Add a cluster…", add.clone()).kind(Kind::Primary),
                            { let r = running.clone(); MenuItem::new("Run action…", Callback::from(move |_| r.set(true))) },
                        ]} />
                    }
                </> }} />
            <Panel>{ body }</Panel>
            if auth.is_admin() && *running {
                <RunAction on_close={{ let r = running.clone(); Callback::from(move |_| r.set(false)) }} />
            }
            if auth.is_admin() && *adding {
                <super::add_cluster::AddCluster on_close={{
                    let (a, reload) = (adding.clone(), fleet.reload.clone());
                    Callback::from(move |_| { a.set(false); reload.emit(()); })
                }} />
            }
        </div>
    }
}

/// The pills a cluster row carries: its health once read, then only the
/// unusual (deploying, paused, drifted).
pub fn cluster_state_pills(c: &HostSummary) -> Html {
    let status = c.cluster_status.clone().unwrap_or_default();
    html! { <>
        if c.health != HealthSummary::Unknown {
            <Pill tone={health_tone(c.health)} label={health_label(c.health)} />
        }
        if !status.deploying.is_empty() {
            <Pill tone={Tone::Active} label="deploying" title={deploying_text(&status.deploying)} />
        }
        if c.status != HostStatus::Active {
            <Pill tone={host_status_tone(c.status)} label={host_status_label(c.status)} />
        }
        if c.drifted { <Pill tone={Tone::Warn} label="drifted" /> }
    </> }
}

/// The host speaking for a cluster, with its card, and whether it is gone.
pub fn steward_cell(
    status: &ClusterStatus,
    hosts: &BTreeMap<&str, &HostSummary>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    match status.steward.as_deref() {
        Some(e) => {
            let host = hosts.get(e).copied();
            html! { <>
                <HostLink id={e.to_owned()} host={host.cloned()} {now} classes={classes!("mono")} />
                if status.reachable == Some(false) {
                    <span class="cell-sub text-warn" title={status.reachable_error.clone()}>{ "cannot reach the cluster" }</span>
                }
            </> }
        }
        None => {
            html! { <span class="muted" title="No connected agent its executors selector matches holds its lease.">{ "none yet" }</span> }
        }
    }
}

fn row(
    c: &HostSummary,
    hosts: &BTreeMap<&str, &HostSummary>,
    stable: Option<&str>,
    on_open: &Callback<(MouseEvent, String)>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let spec = c.cluster.as_ref().map(|i| &i.spec);
    let status = c.cluster_status.clone().unwrap_or_default();
    let onclick = {
        let (on_open, id) = (on_open.clone(), c.id.clone());
        Callback::from(move |e: MouseEvent| on_open.emit((e, id.clone())))
    };
    let kind = spec.map(|s| s.kind.as_str()).unwrap_or("cluster");
    html! {
        <tr data-host={c.id.clone()} class="row-click" {onclick}>
            <td class="nowrap">
                <HostLink id={c.id.clone()} host={c.clone()} {now} />
                <span class="cell-sub">
                    { kind }
                    if let Some(parent) = spec.and_then(|s| s.cluster.as_deref()) { { format!(" · part of {parent}") } }
                    if !c.cluster.as_ref().is_some_and(|i| i.declared) { { " · added through the API" } }
                </span>
            </td>
            <td class="cell-state nowrap">{ cluster_state_pills(c) }</td>
            <td>
                { count(status.workloads as usize, "workload", "workloads") }
                if !status.failing.is_empty() {
                    <span class="cell-sub text-danger">{ format!("failing: {}", status.failing.join(", ")) }</span>
                }
                if !status.deploying.is_empty() {
                    <span class="cell-sub">{ format!("deploying {}", deploying_text(&status.deploying)) }</span>
                }
            </td>
            <td class="nowrap">{ steward_cell(&status, hosts, now) }</td>
            <td>{ for c.groups.iter().map(|g| html! { <span class="badge">{ g.clone() }</span> }) }</td>
            <td class="nowrap">
                { match &c.applied_revision {
                    Some(r) if Some(r.as_str()) == stable => html! {
                        <span class="mono muted" title={format!("{r} (stable)")}>{ commit(r) }{ " " }<span class="stable-mark">{ icons::check() }</span></span>
                    },
                    Some(r) => html! { <span class="mono revision-other" title={format!("{r} (not the stable revision)")}>{ commit(r) }</span> },
                    None => html! { <span class="muted">{ "nothing yet" }</span> },
                } }
            </td>
            <td class="nowrap" title={c.health_at.map(timestamp)}>{ relative_opt(c.health_at, now) }</td>
        </tr>
    }
}
