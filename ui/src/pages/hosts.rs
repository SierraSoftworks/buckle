//! Hosts (design §12): every agent on one line: its state (health, or
//! offline, plus anything out of the ordinary), effective labels, platform,
//! applied revision (quiet when stable) and last contact. Hover a name for
//! the host's card, click a row to open it. Pending registrations come first
//! so they are approved rather than forgotten. Admins can add an agent (the
//! bootstrap wizard, [`super::add_agent`]) and upgrade every reachable agent
//! behind the server's version at once.

use buckle_api::{Connection, HostStatus, HostSummary};
use serde::{Deserialize, Serialize};
use yew::prelude::*;
use yew_router::prelude::*;
use yewi::{Banner, NavTarget, Size, TabLink, TabNav, TabsVariant, Tone, attrs};

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Confirm, Empty, ErrorState, HostLink, Kind, Loading, MenuItem, PageHead, Refresh,
    RouteLink, RunAction, SplitButton, host_state_pills, icons, use_toast,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{agents_behind, expected_version, matches_filter, same_version};
use crate::logic::format::{age, commit, count, relative, timestamp};
use crate::logic::live::apply_fleet;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    show: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    q: String,
    /// The Add an agent ticket Tailscale returned with.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    join: String,
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

/// A query as the pairs of its URL.
fn pairs(q: &Query) -> Vec<(&'static str, String)> {
    [("show", &q.show), ("q", &q.q), ("join", &q.join)]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k, v.clone()))
        .collect()
}

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
    let upgrading = use_state(|| false);
    let adding = use_state(|| false);
    let running = use_state(|| false);
    let busy = use_state(|| false);
    // Back from Tailscale with a ticket: the wizard stays open on it until closed.
    {
        let adding = adding.clone();
        use_effect_with(query.join.is_empty(), move |empty| {
            if !*empty {
                adding.set(true);
            }
        });
    }
    let error = use_state(|| None::<String>);

    let set_query = {
        let navigator = navigator.clone();
        move |q: Query| {
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(&Route::Hosts, &q);
            }
        }
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

    // The version agents converge to, and who is not on it yet.
    let target = live
        .as_ref()
        .and_then(|f| f.desired_agent_version.clone().or(f.server_version.clone()));
    let behind = live
        .as_ref()
        .zip(target.as_deref())
        .map(|(f, t)| agents_behind(&f.hosts, t))
        .unwrap_or_default();
    let upgrade_ids: Vec<String> = behind.upgradable.iter().map(|h| h.id.clone()).collect();
    // Nobody left to upgrade (here or elsewhere): the dialog closes for good.
    {
        let upgrading = upgrading.clone();
        use_effect_with(upgrade_ids.is_empty(), move |empty| {
            if *empty {
                upgrading.set(false);
            }
        });
    }

    let upgrade_all = {
        let (upgrading, busy, toast, reload) = (
            upgrading.clone(),
            busy.clone(),
            toast.clone(),
            fleet.reload.clone(),
        );
        let (ids, target) = (upgrade_ids.clone(), target.clone().unwrap_or_default());
        Callback::from(move |_note: String| {
            let (upgrading, busy, toast, reload) = (
                upgrading.clone(),
                busy.clone(),
                toast.clone(),
                reload.clone(),
            );
            let (ids, target) = (ids.clone(), target.clone());
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let results = futures::future::join_all(
                    ids.iter()
                        .map(|id| api::upgrade_host(id, Some(target.clone()))),
                )
                .await;
                let failed: Vec<String> = results
                    .into_iter()
                    .filter_map(|r| r.err().map(|e| e.to_string()))
                    .collect();
                let asked = ids.len() - failed.len();
                if asked > 0 {
                    toast.ok(format!(
                        "{} will upgrade to {target} once idle.",
                        count(asked, "agent", "agents")
                    ));
                }
                if !failed.is_empty() {
                    toast.error(format!("Not upgraded: {}", failed.join(" ")));
                }
                busy.set(false);
                upgrading.set(false);
                reload.emit(());
            });
        })
    };

    // A click anywhere on a row opens the host, except on a control of its own.
    let on_open = {
        let navigator = navigator.clone();
        Callback::from(move |(e, id): (MouseEvent, String)| {
            let own = e
                .target_dyn_into::<web_sys::Element>()
                .and_then(|t| t.closest("a, button, input").ok().flatten())
                .is_some();
            if let (false, Some(n)) = (own, &navigator) {
                n.push(&Route::Host { id });
            }
        })
    };

    let body = match (&live, &fleet.error) {
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={fleet.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading hosts…" /> },
        (Some(f), _) if f.hosts.is_empty() => html! {
            <Empty title="No hosts have enrolled"
                message="Add an agent to get the one command that installs Buckle on a host and enrols it.">
                if auth.is_admin() {
                    <Button kind={Kind::Primary} onclick={{ let a = adding.clone(); Callback::from(move |_| a.set(true)) }}>{ "Add an agent…" }</Button>
                }
            </Empty>
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
            let tabs = FILTERS.iter().map(|(key, label)| {
                let n = f.hosts.iter().filter(|h| matches_filter(h, key)).count();
                let to = Query { show: if *key == "all" { String::new() } else { key.to_string() }, ..query.clone() };
                html! {
                    <TabLink href={NavTarget::route(Route::Hosts).query(pairs(&to))} current={*key == show}
                        count={n.to_string()}>
                        { *label }
                    </TabLink>
                }
            });
            html! { <>
                if !pending.is_empty() {
                    <Banner tone={Tone::Info} class="pending" attrs={attrs! { "data-testid" => "pending-registrations" }}>
                        <div class="pending__body">
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
                    </Banner>
                }
                <div class="toolbar">
                    <TabNav label="Show" variant={TabsVariant::Pill} size={Size::Small}>{ for tabs }</TabNav>
                </div>
                if rows.is_empty() {
                    <Empty title="No hosts match" message="Clear the search or choose another filter." />
                } else {
                    <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true attrs={yewi::attrs! { "data-testid" => "hosts-table" }}>
                            <thead><tr>
                                <th scope="col">{ "Host" }</th>
                                <th scope="col">{ "State" }</th>
                                <th scope="col">{ "Labels" }</th>
                                <th scope="col">{ "Platform" }</th>
                                <th scope="col">{ "Revision" }</th>
                                <th scope="col">{ "Last seen" }</th>
                            </tr></thead>
                            <tbody>{ for rows.into_iter().map(|h| row(h, &Expect {
                                stable: f.stable_revision.as_deref(),
                                agent: expected_version(&f.hosts, target.as_deref()),
                            }, &on_open, now)) }</tbody>
                    </yewi::SimpleTable>
                }
            </> }
        }
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Hosts" subtitle={html! { "Label changes apply to the next rollout's selection." }}
                actions={html! { <>
                    <Refresh loading={fleet.loading} reload={fleet.reload.clone()} />
                    if auth.is_admin() {
                        <SplitButton menu_label="More fleet actions" testid="hosts-actions" items={
                            // Agents left behind are the likeliest next step.
                            (!upgrade_ids.is_empty()).then(|| {
                                let u = upgrading.clone();
                                MenuItem::new("Upgrade all agents…", Callback::from(move |_| u.set(true)))
                            })
                            .into_iter()
                            .chain([
                                { let a = adding.clone(); MenuItem::new("Add an agent…", Callback::from(move |_| a.set(true))) },
                                { let r = running.clone(); MenuItem::new("Run action…", Callback::from(move |_| r.set(true))) },
                            ])
                            .enumerate()
                            .map(|(i, item)| if i == 0 { item.kind(Kind::Primary) } else { item })
                            .collect::<Vec<_>>()
                        } />
                    }
                </> }} />
            { body }
            if auth.is_admin() && *running {
                <RunAction on_close={{ let r = running.clone(); Callback::from(move |_| r.set(false)) }} />
            }
            if auth.is_admin() && *adding {
                <super::add_agent::AddAgent
                    hosts={live.as_ref().map(|f| f.hosts.clone()).unwrap_or_default()}
                    resume={(!query.join.is_empty()).then(|| query.join.clone())}
                    on_close={{
                        let (adding, set_query, query) = (adding.clone(), set_query.clone(), query.clone());
                        Callback::from(move |_| {
                            adding.set(false);
                            if !query.join.is_empty() {
                                set_query(Query { join: String::new(), ..query.clone() });
                            }
                        })
                    }} />
            }
            if let Some(id) = (*approving).clone() {
                <Confirm title={format!("Approve {id}")} confirm_label={format!("Approve {id}")}
                    busy={*busy} error={(*error).clone()} on_confirm={approve}
                    on_cancel={{ let a = approving.clone(); let e = error.clone(); Callback::from(move |_| { a.set(None); e.set(None); }) }}>
                    <p>{ format!("{id} enrolled and is waiting to join the fleet. Once approved it is assigned the stable revision and is selected by the next rollout like any other host.") }</p>
                    <p class="muted">{ "Check the host's labels and facts first if you did not enrol it yourself." }</p>
                </Confirm>
            }
            if *upgrading && !upgrade_ids.is_empty() {
                <Confirm title={format!("Upgrade agents to {}", target.clone().unwrap_or_default())}
                    confirm_label={format!("Upgrade {}", count(upgrade_ids.len(), "agent", "agents"))}
                    busy={*busy} on_confirm={upgrade_all}
                    on_cancel={{ let u = upgrading.clone(); Callback::from(move |_| u.set(false)) }}>
                    <p>{ "Each agent downloads the release from GitHub, verifies its digest and swaps its binary once nothing else is queued, then restarts into it." }</p>
                    <ul class="confirm__facts" data-testid="upgrade-all-hosts">
                        { for behind.upgradable.iter().map(|h| html! {
                            <li><span class="mono">{ h.id.clone() }</span>{ format!(" from {}", h.agent_version.as_deref().unwrap_or("an unknown version")) }</li>
                        }) }
                    </ul>
                    if !behind.offline.is_empty() {
                        <p class="muted">{ format!(
                            "Skipped while offline: {}. Upgrade all again once they reconnect.",
                            behind.offline.iter().map(|h| h.id.as_str()).collect::<Vec<_>>().join(", "),
                        ) }</p>
                    }
                </Confirm>
            }
        </div>
    }
}

/// What a row is compared against: anything else is called out.
struct Expect<'a> {
    stable: Option<&'a str>,
    agent: Option<&'a str>,
}

fn row(
    h: &HostSummary,
    expect: &Expect<'_>,
    on_open: &Callback<(MouseEvent, String)>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let offline = h.connection == Connection::Offline;
    let platform = [h.platform.as_deref(), h.arch.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    // The agent's version only where it is not the one agents converge to.
    let skew = h
        .agent_version
        .as_deref()
        .filter(|v| expect.agent.is_some_and(|e| !same_version(e, v)));
    let onclick = {
        let (on_open, id) = (on_open.clone(), h.id.clone());
        Callback::from(move |e: MouseEvent| on_open.emit((e, id.clone())))
    };
    html! {
        <tr data-host={h.id.clone()} class="yi-table__row yi-table__row--clickable" {onclick}>
            <td class="nowrap"><HostLink id={h.id.clone()} host={h.clone()} {now} /></td>
            <td class="cell-state nowrap" title={h.health_at.map(|at| format!("health read {} ago ({})", age(Some(at), now), timestamp(at)))}>
                { host_state_pills(h) }
                // A reading is only worth a line of its own once it is old.
                if let Some(at) = h.health_at.filter(|at| !offline && now - *at > chrono::Duration::hours(1)) {
                    <span class="cell-sub text-warn" title={timestamp(at)}>{ format!("read {} ago", age(Some(at), now)) }</span>
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
            <td class="nowrap">
                <span title={h.agent_version.as_ref().map(|v| format!("agent {v}"))}>{ platform }</span>
                if let Some(v) = skew {
                    <span class="cell-sub text-warn" title={format!("expected {}", expect.agent.unwrap_or_default())}>{ format!("agent {v}") }</span>
                }
                if let Some(u) = &h.upgrade {
                    <span class="cell-sub" title={u.error.clone()}>{ match u.state {
                        buckle_api::UpgradeState::Requested => format!("upgrading to {}", u.version),
                        buckle_api::UpgradeState::Failed => format!("upgrade to {} failed", u.version),
                    } }</span>
                }
            </td>
            <td class="nowrap">
                { match &h.applied_revision {
                    // The stable revision is the norm: shown quietly, so a
                    // host on anything else stands out.
                    Some(r) if Some(r.as_str()) == expect.stable => html! {
                        <span class="mono muted" title={format!("{r} (stable)")}>{ commit(r) }{ " " }<span class="stable-mark">{ icons::check() }</span></span>
                    },
                    Some(r) => html! { <span class="mono revision-other" title={format!("{r} (not the stable revision)")}>{ commit(r) }</span> },
                    None => html! { <span class="muted">{ "—" }</span> },
                } }
            </td>
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
