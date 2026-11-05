//! A cluster's page: what the host page is for a host, with workloads in
//! place of modules. Its summary (scheduler, API, nodes, the host speaking
//! for it); notes and labels beside the agents able to run its operations;
//! one Workloads card (how each reads, what the latest run did to it, a
//! link into the cluster's own UI per job, and opened: its checks, that
//! run's steps and its resources); recent runs; and its audit trail.
//! Allocation-level detail stays in the cluster's UI.

use std::collections::BTreeMap;

use buckle_api::{Connection, HostStatus, HostSummary};
use yew::prelude::*;
use yew_router::prelude::*;

use super::clusters::{cluster_state_pills, executor_cell};
use super::host::{LabelsEditor, NotesEditor, runs};
use super::modules_card::ModulesPanel;
use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    ActionsButton, Button, Confirm, ErrorState, Kind, Loading, PageHead, Panel, Pill, Refresh,
    RouteLink, icons, use_target_actions, use_toast,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::format::{commit, relative_opt, timestamp};
use crate::logic::live::{apply_fleet, apply_host_detail};
use crate::logic::run::newest_run;
use crate::logic::tone::Tone;
use crate::store::use_live;

#[derive(Properties, PartialEq)]
pub struct Props {
    pub id: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    Pause,
    Resume,
    Forget,
}

#[function_component(ClusterPage)]
pub fn cluster_page(props: &Props) -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let toast = use_toast();
    let navigator = use_navigator();
    let detail = use_fetch(props.id.clone(), |id| async move { api::host(&id).await });
    let live = use_live_value(&detail, apply_host_detail);
    let fleet = use_fetch((), |_| api::fleet());
    let fleet = use_live_value(&fleet, apply_fleet);
    let ask = use_state(|| None::<Ask>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);
    let picked = use_state_eq(|| None::<String>);
    {
        let picked = picked.clone();
        use_effect_with(props.id.clone(), move |_| {
            picked.set(None);
            || ()
        });
    }
    // An action that started: follow the newest run, its own.
    let target_actions = {
        let (picked, reload) = (picked.clone(), detail.reload.clone());
        use_target_actions(
            props.id.clone(),
            Callback::from(move |_| {
                picked.set(None);
                reload.emit(());
            }),
        )
    };
    let store = use_live();
    let newest = live.as_ref().and_then(|d| {
        let events = detail
            .data
            .as_ref()
            .and_then(|f| store.log.since(f.0))
            .unwrap_or_default();
        newest_run(
            d.latest_run.as_ref().map(|r| r.id.as_str()),
            &d.summary.id,
            events.iter().map(|e| &**e),
        )
    });

    let Some(d) = live else {
        return html! {
            <div class="page">
                <PageHead title={props.id.clone()} />
                { match &detail.error {
                    Some(e) => html! { <ErrorState error={e.clone()} retry={detail.reload.clone()} /> },
                    None => html! { <Loading label="Loading the cluster…" /> },
                } }
            </div>
        };
    };
    let c = &d.summary;
    // A host has a page of its own.
    if c.kind != buckle_api::TargetKind::Cluster {
        return html! { <Redirect<Route> to={Route::Host { id: c.id.clone() }} /> };
    }
    let admin = auth.is_admin();
    let info = c.cluster.clone();
    let declared = info.as_ref().is_some_and(|i| i.declared);
    let status = c.cluster_status.clone().unwrap_or_default();
    let hosts: Vec<HostSummary> = fleet.as_ref().map(|f| f.hosts.clone()).unwrap_or_default();
    let by_id: BTreeMap<&str, &HostSummary> = hosts.iter().map(|h| (h.id.as_str(), h)).collect();
    let active = fleet.as_ref().and_then(|f| f.active_rollout.clone());
    // Actions run on an active cluster through its connected executor, and
    // only for admins.
    let can_act = admin
        && c.status == HostStatus::Active
        && status
            .executor
            .as_deref()
            .and_then(|e| by_id.get(e))
            .is_some_and(|h| h.connection == Connection::Online);
    let on_action = can_act.then(|| target_actions.on_action.clone());

    let open = |a: Ask| {
        let (ask, error) = (ask.clone(), error.clone());
        Callback::from(move |_: MouseEvent| {
            error.set(None);
            ask.set(Some(a));
        })
    };
    let run = {
        let (ask, busy, error, toast, reload, id) = (
            ask.clone(),
            busy.clone(),
            error.clone(),
            toast.clone(),
            detail.reload.clone(),
            c.id.clone(),
        );
        let navigator = navigator.clone();
        Callback::from(move |_note: String| {
            let Some(what) = *ask else { return };
            let (ask, busy, error, toast, reload, id, navigator) = (
                ask.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                reload.clone(),
                id.clone(),
                navigator.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let result = match what {
                    Ask::Pause => api::host_action(&id, "pause")
                        .await
                        .map(|_| format!("{id} paused: excluded from rollouts.")),
                    Ask::Resume => api::host_action(&id, "resume")
                        .await
                        .map(|_| format!("{id} resumed.")),
                    Ask::Forget => api::forget_host(&id)
                        .await
                        .map(|_| format!("{id} forgotten.")),
                };
                busy.set(false);
                match result {
                    Ok(message) => {
                        toast.ok(message);
                        ask.set(None);
                        if what == Ask::Forget {
                            if let Some(n) = navigator {
                                n.push(&Route::Clusters);
                            }
                        } else {
                            reload.emit(());
                        }
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
            });
        })
    };

    let actions = html! { <>
        <Refresh loading={detail.loading} reload={detail.reload.clone()} />
        if let Some(on_run) = &on_action {
            <ActionsButton actions={d.actions.clone()} on_run={on_run.clone()} testid="cluster-actions" />
        }
        if admin {
            if c.status == HostStatus::Paused {
                <Button onclick={open(Ask::Resume)}>{ "Resume" }</Button>
            } else {
                <Button onclick={open(Ask::Pause)}>{ "Pause…" }</Button>
            }
            // A declared cluster follows the repository: it goes when
            // buckle.yaml stops declaring it.
            if !declared {
                <Button kind={Kind::Danger} onclick={open(Ask::Forget)}>{ "Forget…" }</Button>
            }
        }
    </> };
    let kind = info
        .as_ref()
        .map(|i| i.spec.kind.as_str())
        .unwrap_or("cluster");
    let subtitle = html! {
        <div class="rollout-meta">
            <Pill tone={Tone::Info} label={format!("{kind} cluster")}
                title={if declared { "declared in buckle.yaml" } else { "created through the API" }} />
            { cluster_state_pills(c) }
            <span class="muted">
                { match &status.executor {
                    Some(e) => html! { <>{ "executor " }<span class="mono">{ e.clone() }</span></> },
                    None => html! { "no executor yet" },
                } }
                { format!(" · health read {}", relative_opt(c.health_at, now)) }
            </span>
        </div>
    };

    let dialog = ask.as_ref().map(|a| {
        let (title, body, danger, typed): (String, Html, bool, Option<AttrValue>) = match a {
            Ask::Pause => (format!("Pause {}", c.id), html! {
                <p>{ "A paused cluster stays listed but rollouts leave it out until it is resumed. Its workloads keep running." }</p>
            }, false, None),
            Ask::Resume => (format!("Resume {}", c.id), html! {
                <p>{ "The cluster rejoins rollouts." }</p>
            }, false, None),
            Ask::Forget => (format!("Forget {}", c.id), html! { <>
                <p>{ "Forgetting removes the cluster target and its history from the server: runs and health. Nothing in the cluster changes; its workloads keep running." }</p>
            </> }, true, Some(AttrValue::from(c.id.clone()))),
        };
        html! {
            <Confirm title={title.clone()} confirm_label={title} {danger} type_to_confirm={typed} note=true busy={*busy}
                error={(*error).clone()} on_confirm={run.clone()} on_cancel={{ let ask = ask.clone(); Callback::from(move |_| ask.set(None)) }}>
                { body }
            </Confirm>
        }
    });

    let spec = info.as_ref().map(|i| i.spec.clone());
    let summary = crate::components::ui::facts_list(vec![
        (
            "Cluster",
            html! { <>
                { kind.to_string() }
                if let Some(d) = spec.as_ref().map(|s| s.description.clone()).filter(|d| !d.is_empty()) {
                    <span class="muted">{ format!(" · {d}") }</span>
                }
            </> },
        ),
        (
            "API",
            match spec.as_ref().and_then(|s| s.api.clone()) {
                Some(api) => html! { <span class="mono">{ api }</span> },
                None => html! { <span class="muted">{ "the executor's local default" }</span> },
            },
        ),
        (
            "Nodes",
            match spec.as_ref().and_then(|s| s.nodes.clone()) {
                Some(n) => html! { <code class="mono small">{ n }</code> },
                None => match spec.as_ref().and_then(|s| s.cluster.clone()) {
                    Some(parent) => {
                        html! { <>{ "part of " }<RouteLink to={Route::Cluster { id: parent.clone() }} classes="mono">{ parent }</RouteLink></> }
                    }
                    None => html! { <span class="muted">{ "none selected" }</span> },
                },
            },
        ),
        (
            "Executor",
            html! { <>
                { executor_cell(status.executor.as_deref(), &by_id, now) }
                if let Some(sel) = spec.as_ref().and_then(|s| s.executors.clone()) {
                    <span class="cell-sub">{ "chosen from " }<code class="mono small">{ sel }</code></span>
                }
            </> },
        ),
        (
            "Applied",
            match &c.applied_revision {
                Some(r) => html! { <span class="mono" title={r.clone()}>{ commit(r) }</span> },
                None => html! { <span class="muted">{ "nothing yet" }</span> },
            },
        ),
        (
            "Groups",
            html! { <>{ for c.groups.iter().map(|g| html! { <span class="badge">{ g.clone() }</span> }) }</> },
        ),
        (
            "Workloads",
            html! { <>
                { format!("{} managed", status.workloads) }
                if let Some(ui) = spec.as_ref().and_then(|s| s.ui_base().map(str::to_owned)) {
                    { " · " }
                    <a href={ui} target="_blank" rel="noopener noreferrer">{ format!("open {kind}") }{ icons::external() }</a>
                }
            </> },
        ),
        (
            "Health read",
            html! { <span title={c.health_at.map(timestamp)}>{ relative_opt(c.health_at, now) }</span> },
        ),
    ]);

    let executors = html! {
        <div class="cluster-executors" data-testid="cluster-executors">
            <h3 class="subhead">{ "Can run its operations" }</h3>
            if d.executors.is_empty() {
                <p class="muted small">{ match spec.as_ref().and_then(|s| s.executors().map(str::to_owned)) {
                    Some(sel) => html! { <>{ "No active agent matches " }<code class="mono">{ sel }</code>{ ": nothing can apply its workloads or run its actions." }</> },
                    None => html! { "It declares no executors selector: nothing can apply its workloads or run its actions." },
                } }</p>
            } else {
                <p class="muted small">{ "The agents its executors selector matches; a connected one is picked for each rollout or action." }</p>
                <ul class="cluster-executors__list">
                    { for d.executors.iter().map(|e| {
                        let host = by_id.get(e.as_str()).copied();
                        let online = host.is_some_and(|h| h.connection == Connection::Online);
                        let current = status.executor.as_deref() == Some(e.as_str());
                        html! {
                            <li data-host={e.clone()} data-online={online.to_string()}>
                                <crate::components::HostLink id={e.clone()} host={host.cloned()} {now} classes={classes!("mono")} />
                                if current { <Pill tone={Tone::Info} label="last executor" /> }
                                if !online { <span class="muted small">{ "offline" }</span> }
                            </li>
                        }
                    }) }
                </ul>
            }
        </div>
    };
    let show_run = {
        let (picked, newest) = (picked.clone(), newest.clone());
        Callback::from(move |run: String| {
            picked.set((Some(&run) != newest.as_ref()).then_some(run));
            if let Some(el) = crate::util::window()
                .document()
                .and_then(|d| d.get_element_by_id("workloads"))
            {
                el.scroll_into_view();
            }
        })
    };
    let shown = (*picked).clone().or(newest.clone());

    html! {
        <div class="page page--wide" data-testid="cluster-page">
            <PageHead title={c.id.clone()} {subtitle} {actions} />
            <div class="grid grid--host">
                <Panel title="Summary">{ summary }</Panel>
                <Panel title="Notes and labels" class="notes-labels">
                    <NotesEditor id={c.id.clone()} notes={c.notes.clone()} admin={admin} on_saved={detail.reload.clone()} embedded=true />
                    <LabelsEditor detail={d.clone()} admin={admin} rollout_active={active.is_some()} on_saved={detail.reload.clone()}
                        embedded=true aside={executors} />
                </Panel>
            </div>
            <ModulesPanel detail={d.clone()} cluster=true on_action={on_action.clone()} newest={newest.clone()} picked={(*picked).clone()}
                on_pick={{ let picked = picked.clone(); Callback::from(move |run| picked.set(run)) }} />
            <Panel title="Recent runs">{ runs(&d, now, shown, &show_run) }</Panel>
            <p class="muted small">
                <RouteLink to={Route::Audit} query={vec![("filter".to_string(), crate::logic::filter::equals("subject", &c.id))]}>
                    { format!("Audit entries for {}", c.id) }
                </RouteLink>
            </p>
            { dialog.unwrap_or_default() }
            { target_actions.dialog }
        </div>
    }
}
