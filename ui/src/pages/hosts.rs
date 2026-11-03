//! Hosts (design §12): every agent with its effective labels, groups, facts
//! summary, status, applied revision, drift, health and last contact, with
//! pending registrations first so they are approved rather than forgotten.

use buckle_api::{Connection, HostStatus, HostSummary};
use serde::{Deserialize, Serialize};
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Confirm, Empty, ErrorState, Kind, Loading, PageHead, Panel, Pill, Refresh, RouteLink,
    use_toast,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::matches_filter;
use crate::logic::format::{age, commit, relative, timestamp};
use crate::logic::live::apply_fleet;
use crate::logic::tone::{Tone, health_label, health_tone, host_status_label, host_status_tone};

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    show: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    q: String,
}

const FILTERS: &[(&str, &str)] = &[
    ("all", "All"),
    ("pending", "Pending"),
    ("healthy", "Healthy"),
    ("failing", "Failing"),
    ("held", "Held"),
    ("offline", "Offline"),
    ("drifted", "Drifted"),
    ("paused", "Paused"),
];

fn search_matches(h: &HostSummary, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty()
        || h.id.to_lowercase().contains(&q)
        || h.groups.iter().any(|g| g.to_lowercase().contains(&q))
        || h.effective_labels
            .iter()
            .any(|(k, v)| format!("{k}={v}").to_lowercase().contains(&q))
        || h.platform.as_deref().is_some_and(|p| p.contains(&q))
}

#[function_component(Hosts)]
pub fn hosts() -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let toast = use_toast();
    let location = use_location();
    let navigator = use_navigator();
    let query: Query = location
        .and_then(|l| l.query::<Query>().ok())
        .unwrap_or_default();
    let fleet = use_fetch((), |_| api::fleet());
    let live = use_live_value(&fleet, apply_fleet);
    let approving = use_state(|| None::<String>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let set_query = {
        let navigator = navigator.clone();
        move |q: Query| {
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(&Route::Hosts, &q);
            }
        }
    };
    let on_search = {
        let (set_query, query) = (set_query.clone(), query.clone());
        Callback::from(move |e: InputEvent| {
            let value = e.target_unchecked_into::<HtmlInputElement>().value();
            set_query(Query {
                q: value,
                ..query.clone()
            });
        })
    };

    let approve = {
        let (approving, busy, error, toast, reload) = (
            approving.clone(),
            busy.clone(),
            error.clone(),
            toast.clone(),
            fleet.reload.clone(),
        );
        Callback::from(move |_note: String| {
            let Some(id) = (*approving).clone() else {
                return;
            };
            let (approving, busy, error, toast, reload) = (
                approving.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                reload.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::host_action(&id, "approve").await {
                    Ok(_) => {
                        toast.ok(format!(
                            "{id} approved: it joins the next rollout's selection."
                        ));
                        approving.set(None);
                        reload.emit(());
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
                busy.set(false);
            });
        })
    };

    let body = match (&live, &fleet.error) {
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={fleet.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading hosts…" /> },
        (Some(f), _) if f.hosts.is_empty() => html! {
            <Empty title="No hosts have enrolled"
                message="Install the agent on a host and enrol it with a token or over Tailscale; it appears here awaiting approval." />
        },
        (Some(f), _) => {
            let show = if query.show.is_empty() {
                "all"
            } else {
                query.show.as_str()
            };
            let mut rows: Vec<&HostSummary> = f
                .hosts
                .iter()
                .filter(|h| matches_filter(h, show) && search_matches(h, &query.q))
                .collect();
            rows.sort_by(|a, b| {
                (a.status != HostStatus::Pending, &a.id)
                    .cmp(&(b.status != HostStatus::Pending, &b.id))
            });
            let pending: Vec<&HostSummary> = f
                .hosts
                .iter()
                .filter(|h| h.status == HostStatus::Pending)
                .collect();
            let chips = FILTERS.iter().map(|(key, label)| {
                let n = f.hosts.iter().filter(|h| matches_filter(h, key)).count();
                let selected = *key == show;
                let (set_query, query) = (set_query.clone(), query.clone());
                let key = key.to_string();
                html! {
                    <button type="button" class={classes!("chip", selected.then_some("chip--active"))}
                        aria-pressed={selected.to_string()}
                        onclick={Callback::from(move |_| set_query(Query { show: key.clone(), ..query.clone() }))}>
                        { *label }<span class="chip__count">{ n }</span>
                    </button>
                }
            });
            html! { <>
                if !pending.is_empty() {
                    <div class="banner banner--info" role="status" data-testid="pending-registrations">
                        <strong>{ format!("{} awaiting registration approval: ", pending.len()) }</strong>
                        { for pending.iter().map(|h| {
                            let id = h.id.clone();
                            let approving = approving.clone();
                            html! {
                                <span class="banner__item">
                                    <RouteLink to={Route::Host { id: h.id.clone() }} classes="mono">{ h.id.clone() }</RouteLink>
                                    <span class="muted small">{ format!(" enrolled {}", relative(h.first_seen, now)) }</span>
                                    if auth.is_admin() {
                                        <Button small=true kind={Kind::Primary} onclick={Callback::from(move |_| approving.set(Some(id.clone())))}>{ "Approve…" }</Button>
                                    }
                                </span>
                            }
                        }) }
                    </div>
                }
                <div class="toolbar">
                    <div class="chips" role="group" aria-label="Show">{ for chips }</div>
                    <input class="input toolbar__search" type="search" placeholder="Search hosts, labels, groups"
                        aria-label="Search hosts" value={query.q.clone()} oninput={on_search} />
                </div>
                if rows.is_empty() {
                    <Empty title="No hosts match" message="Clear the search or choose another filter." />
                } else {
                    <div class="table-wrap">
                        <table class="table" data-testid="hosts-table">
                            <thead><tr>
                                <th scope="col">{ "Host" }</th>
                                <th scope="col">{ "Status" }</th>
                                <th scope="col">{ "Health" }</th>
                                <th scope="col">{ "Labels" }</th>
                                <th scope="col">{ "Groups" }</th>
                                <th scope="col">{ "Facts" }</th>
                                <th scope="col">{ "Revision" }</th>
                                <th scope="col">{ "Drift" }</th>
                                <th scope="col">{ "Last seen" }</th>
                            </tr></thead>
                            <tbody>{ for rows.into_iter().map(|h| row(h, f.stable_revision.as_deref(), now)) }</tbody>
                        </table>
                    </div>
                }
            </> }
        }
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Hosts" subtitle={html! { "Every enrolled agent. Label changes apply to the next rollout's selection." }}
                actions={html! { <Refresh loading={fleet.loading} reload={fleet.reload.clone()} /> }} />
            <Panel>{ body }</Panel>
            if let Some(id) = (*approving).clone() {
                <Confirm title={format!("Approve {id}")} confirm_label={format!("Approve {id}")}
                    busy={*busy} error={(*error).clone()} on_confirm={approve}
                    on_cancel={{ let a = approving.clone(); let e = error.clone(); Callback::from(move |_| { a.set(None); e.set(None); }) }}>
                    <p>{ format!("{id} enrolled and is waiting to join the fleet. Once approved it is assigned the stable revision and is selected by the next rollout like any other host.") }</p>
                    <p class="muted">{ "Check the host's labels and facts first if you did not enrol it yourself." }</p>
                </Confirm>
            }
        </div>
    }
}

fn row(h: &HostSummary, stable: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Html {
    let offline = h.connection == Connection::Offline;
    html! {
        <tr data-host={h.id.clone()}>
            <td class="nowrap"><RouteLink to={Route::Host { id: h.id.clone() }} classes="mono strong">{ h.id.clone() }</RouteLink></td>
            <td class="nowrap"><Pill tone={host_status_tone(h.status)} label={host_status_label(h.status)} /></td>
            <td>
                if offline {
                    <Pill tone={Tone::Neutral} label="offline" title={format!("last reading: {}", health_label(h.health))} />
                } else {
                    <Pill tone={health_tone(h.health)} label={health_label(h.health)} />
                }
                if let Some(at) = h.health_at {
                    <span class="cell-sub" title={timestamp(at)}>{ format!("{} old", age(Some(at), now)) }</span>
                }
            </td>
            <td class="cell-labels">
                { for h.effective_labels.iter().map(|(k, v)| html! {
                    <span class={classes!("label", h.labels.contains_key(k).then_some("label--server"))}
                        title={if h.labels.contains_key(k) { "set by an operator" } else { "reported by the agent" }}>
                        { format!("{k}={v}") }
                    </span>
                }) }
            </td>
            <td>{ for h.groups.iter().map(|g| html! { <span class="badge">{ g.clone() }</span> }) }</td>
            <td class="nowrap">
                <span>{ h.platform.clone().unwrap_or_default() }</span>
                <span class="cell-sub">{ h.arch.clone().unwrap_or_default() }</span>
                <span class="cell-sub" title="agent version">{ h.agent_version.clone().map(|v| format!("agent {v}")).unwrap_or_default() }</span>
                if let Some(u) = &h.upgrade {
                    <span class="cell-sub" title={u.error.clone()}>{ match u.state {
                        buckle_api::UpgradeState::Requested => format!("upgrading to {}", u.version),
                        buckle_api::UpgradeState::Failed => format!("upgrade to {} failed", u.version),
                    } }</span>
                }
            </td>
            <td>
                { match &h.applied_revision {
                    Some(r) => html! { <>
                        <span class="mono" title={r.clone()}>{ commit(r) }</span>
                        if Some(r.as_str()) == stable { <span class="badge badge--ok">{ "stable" }</span> }
                    </> },
                    None => html! { <span class="muted">{ "—" }</span> },
                } }
            </td>
            <td>{ if h.drifted { html! { <Pill tone={Tone::Warn} label="drifted" /> } } else { html! { <span class="muted">{ "—" }</span> } } }</td>
            <td class="nowrap" title={h.last_seen.map(timestamp)}>
                if offline { <span class="text-warn">{ relative_opt(h.last_seen, now) }</span> } else { { relative_opt(h.last_seen, now) } }
            </td>
        </tr>
    }
}

fn relative_opt(
    at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    at.map(|a| relative(a, now))
        .unwrap_or_else(|| "never".into())
}
