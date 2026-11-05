//! Host detail (design §12): the facts tree, the host's blueprint (what
//! Buckle manages there, module by module), each health check's current
//! reading and its worst in recent rollouts, drift diffs, the latest run
//! module by module and step by step (live while it applies, with any older
//! run a pick away), and, for admins, the controls: server labels with a live preview of the
//! groups and phase they produce, registration approval, pause and resume,
//! agent upgrades, notes, and forget.

use std::collections::BTreeMap;

use buckle_api::{
    AgentUpgrade, Connection, HostCheck, HostDetail, HostPatch, HostPreview, HostStatus,
    UpgradeState,
};
use chrono::{DateTime, Utc};
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::components::ui::{Note, NoteKind};
use yew_router::prelude::*;

use super::modules_card::ModulesPanel;
use crate::api::{self, ApiError};
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Confirm, DiffView, Empty, ErrorBlock, ErrorState, JsonTree, Kind, Loading, MenuItem,
    PageHead, Panel, Pill, Refresh, RouteLink, SplitButton, TraceLink, action_items,
    changes::{RunChanges, Took},
    host_state_pills, target_items, use_target_actions, use_toast,
};
use crate::hooks::{use_debounced, use_fetch, use_live_value, use_now};
use crate::logic::controls::{TargetAction, TargetStanding, head_actions};
use crate::logic::fleet::agents_behind;
use crate::logic::format::{
    commit, relative, relative_opt, rollout_label, short, short_duration, timestamp,
};
use crate::logic::health::CheckState;
use crate::logic::labels::{Change, Row, changes, collect, effective, rows};
use crate::logic::live::{apply_fleet, apply_host_detail};
use crate::logic::run::newest_run;
use crate::logic::tone::{Tone, check_status_tone, outcome_tone};
use crate::store::use_live;

#[derive(Properties, PartialEq)]
pub struct Props {
    pub id: String,
}

#[derive(Clone, PartialEq)]
enum Ask {
    Approve,
    Pause,
    Resume,
    Upgrade,
    Forget,
}

#[function_component(HostPage)]
pub fn host_page(props: &Props) -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let toast = use_toast();
    let navigator = use_navigator();
    let detail = use_fetch(props.id.clone(), |id| async move { api::host(&id).await });
    let live = use_live_value(&detail, apply_host_detail);
    let fleet = use_fetch((), |_| api::fleet());
    let fleet = use_live_value(&fleet, apply_fleet);
    let active = fleet.as_ref().and_then(|f| f.active_rollout.clone());
    let desired = fleet
        .as_ref()
        .and_then(|f| f.desired_agent_version.clone().or(f.server_version.clone()));
    let review = super::rollout::use_approval_wanted(active.clone(), props.id.clone());
    let ask = use_state(|| None::<Ask>);
    // The version the upgrade dialog will ask for.
    let upgrade_to = use_state(String::new);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);
    // The run the report shows: `None` follows the newest.
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
    // A run that started since the detail was fetched is the newest.
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
                    None => html! { <Loading label="Loading the host…" /> },
                } }
            </div>
        };
    };
    let h = &d.summary;
    // A cluster has a page of its own.
    if h.kind == buckle_api::TargetKind::Cluster {
        return html! { <Redirect<Route> to={Route::Cluster { id: h.id.clone() }} /> };
    }
    let admin = auth.is_admin();

    let open = |a: Ask| {
        let (ask, error, upgrade_to) = (ask.clone(), error.clone(), upgrade_to.clone());
        let desired = desired.clone().unwrap_or_default();
        Callback::from(move |_: MouseEvent| {
            error.set(None);
            if a == Ask::Upgrade {
                upgrade_to.set(desired.clone());
            }
            ask.set(Some(a.clone()));
        })
    };
    let close = {
        let ask = ask.clone();
        Callback::from(move |_| ask.set(None))
    };
    let run = {
        let (ask, busy, error, toast, reload, id) = (
            ask.clone(),
            busy.clone(),
            error.clone(),
            toast.clone(),
            detail.reload.clone(),
            h.id.clone(),
        );
        let navigator = navigator.clone();
        let upgrade_to = upgrade_to.clone();
        Callback::from(move |_note: String| {
            let Some(what) = (*ask).clone() else { return };
            let version = (*upgrade_to).trim().to_owned();
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
            let forgot = what == Ask::Forget;
            wasm_bindgen_futures::spawn_local(async move {
                let result = match what {
                    Ask::Approve => api::host_action(&id, "approve")
                        .await
                        .map(|_| format!("{id} approved.")),
                    Ask::Pause => api::host_action(&id, "pause").await.map(|_| {
                        format!("{id} paused: excluded from rollouts and drift correction.")
                    }),
                    Ask::Resume => api::host_action(&id, "resume")
                        .await
                        .map(|_| format!("{id} resumed.")),
                    Ask::Upgrade => {
                        api::upgrade_host(&id, (!version.is_empty()).then_some(version))
                            .await
                            .map(|h| match h.upgrade {
                                Some(u) => format!(
                                    "{id} will upgrade its agent to {} once idle.",
                                    u.version
                                ),
                                None => format!("{id} was asked to upgrade its agent."),
                            })
                    }
                    Ask::Forget => api::forget_host(&id)
                        .await
                        .map(|_| format!("{id} forgotten.")),
                };
                busy.set(false);
                match result {
                    Ok(message) => {
                        toast.ok(message);
                        ask.set(None);
                        if forgot {
                            if let Some(n) = navigator {
                                n.push(&Route::Hosts);
                            }
                        } else {
                            reload.emit(());
                        }
                    }
                    Err(e) => {
                        if e.is_forbidden() {
                            toast.error(format!("Not permitted: {e}"));
                        }
                        error.set(Some(e.to_string()));
                    }
                }
            });
        })
    };

    let on_action = target_actions.on_action.clone();
    // Actions run on an active, online target, and only for admins.
    let can_act = admin && h.status == HostStatus::Active && h.connection == Connection::Online;
    let module_actions = can_act.then(|| on_action.clone());

    let refresh = html! { <Refresh loading={detail.loading} reload={detail.reload.clone()} /> };
    let actions = if admin {
        let (own, primary) = head_actions(TargetStanding {
            pending: h.status == HostStatus::Pending,
            paused: h.status == HostStatus::Paused,
            approval_wanted: review.is_some(),
            upgradable: h.connection == Connection::Online,
            behind: desired.as_deref().is_some_and(|v| {
                !agents_behind(std::slice::from_ref(h), v)
                    .upgradable
                    .is_empty()
            }),
            forgettable: true,
        });
        let modules: Vec<MenuItem> = if can_act {
            action_items(&d.actions, &on_action, Some("Module actions"))
        } else {
            Vec::new()
        };
        let items = target_items(&own, primary, modules, |a| match a {
            TargetAction::ApproveRegistration => open(Ask::Approve),
            TargetAction::ApproveRollout => {
                let (navigator, id) = (navigator.clone(), review.clone().unwrap_or_default());
                Callback::from(move |_: MouseEvent| {
                    if let Some(n) = &navigator {
                        n.push(&Route::rollout(&id));
                    }
                })
            }
            TargetAction::UpgradeAgent => open(Ask::Upgrade),
            TargetAction::Pause => open(Ask::Pause),
            TargetAction::Resume => open(Ask::Resume),
            TargetAction::Forget => open(Ask::Forget),
        });
        html! { <SplitButton {items} menu_label="More host actions" testid="host-actions" /> }
    } else {
        html! {}
    };
    let actions = html! { <>{ refresh }{ actions }</> };

    let offline = h.connection == Connection::Offline;
    let subtitle = html! {
        <div class="rollout-meta">
            { host_state_pills(h) }
            // The clusters whose lease it holds.
            { for fleet.as_ref().into_iter().flat_map(|f| &f.clusters).filter(|c| {
                c.cluster_status.as_ref().and_then(|s| s.steward.as_deref()) == Some(h.id.as_str())
            }).map(|c| html! {
                <RouteLink to={Route::Cluster { id: c.id.clone() }} classes={classes!("pill-link")}>
                    <Pill tone={Tone::Info} label={format!("stewards {}", c.id)}
                        title={format!("It holds {}'s lease: it watches the cluster and runs its workloads and actions.", c.id)} />
                </RouteLink>
            }) }
            <span class="muted">{ format!("{} · last seen {}", if offline { "offline" } else { "online" }, relative_opt(h.last_seen, now)) }</span>
        </div>
    };

    let dialog = ask.as_ref().map(|a| {
        let (title, label, danger, body, typed): (String, String, bool, Html, Option<AttrValue>) = match a {
            Ask::Approve => (format!("Approve {}", h.id), format!("Approve {}", h.id), false,
                html! { <p>{ "The host joins the fleet, is assigned the stable revision and is selected by the next rollout." }</p> }, None),
            Ask::Pause => (format!("Pause {}", h.id), format!("Pause {}", h.id), false,
                html! { <p>{ "A paused host stays in the fleet but is excluded from rollouts and drift correction until resumed." }</p> }, None),
            Ask::Resume => (format!("Resume {}", h.id), format!("Resume {}", h.id), false,
                html! { <p>{ "The host rejoins rollouts and drift correction." }</p> }, None),
            Ask::Upgrade => {
                let oninput = {
                    let upgrade_to = upgrade_to.clone();
                    Callback::from(move |e: InputEvent| upgrade_to.set(e.target_unchecked_into::<HtmlInputElement>().value()))
                };
                (format!("Upgrade {}'s agent", h.id), "Upgrade agent".to_string(), false, html! { <>
                    <p>{ format!(
                        "The agent (now {}) downloads the release from GitHub, verifies its digest and swaps its binary once nothing else is queued, then restarts into it.",
                        h.agent_version.as_deref().unwrap_or("an unknown version"),
                    ) }</p>
                    <label class="field">
                        <span class="field__label">{ "Version" }</span>
                        <input class="input mono" type="text" placeholder={desired.clone().unwrap_or_default()} aria-label="Version"
                            value={(*upgrade_to).clone()} {oninput} spellcheck="false" />
                    </label>
                </> }, None)
            }
            Ask::Forget => (format!("Forget {}", h.id), format!("Forget {}", h.id), true, html! { <>
                <p>{ "Forgetting removes the host and its history from the server: facts, state, runs and health. Nothing on the host itself changes." }</p>
                <p>{ "If the agent is still installed it can enrol again, and will need approving again." }</p>
            </> }, Some(AttrValue::from(h.id.clone()))),
        };
        html! {
            <Confirm {title} confirm_label={label} {danger} type_to_confirm={typed} note=true busy={*busy}
                error={(*error).clone()} on_confirm={run.clone()} on_cancel={close.clone()}>
                { body }
            </Confirm>
        }
    });

    let shown = (*picked).clone().or(newest.clone());

    html! {
        <div class="page page--wide">
            <PageHead title={h.id.clone()} {subtitle} {actions} />
            <div class="grid grid--host">
                <Panel title="Summary" class="panel--sections">
                    { crate::components::ui::facts_list(vec![
                        ("Platform", html! { { format!("{} · {}", h.platform.clone().unwrap_or_default(), h.arch.clone().unwrap_or_default()) } }),
                        ("Agent", agent_version(h.agent_version.as_deref(), h.upgrade.as_ref(), desired.as_deref(), now)),
                        ("Applied", match &h.applied_revision {
                            Some(r) => html! { <span class="mono" title={r.clone()}>{ commit(r) }</span> },
                            None => html! { <span class="muted">{ "nothing yet" }</span> },
                        }),
                        ("Groups", html! { <>{ for h.groups.iter().map(|g| html! { <span class="badge">{ g.clone() }</span> }) }</> }),
                        ("Enrolled", html! { <span title={timestamp(h.first_seen)}>{ relative(h.first_seen, now) }</span> }),
                        ("Health read", html! { <span title={h.health_at.map(timestamp)}>{ relative_opt(h.health_at, now) }</span> }),
                    ]) }
                    <NotesEditor id={h.id.clone()} notes={h.notes.clone()} admin={admin} on_saved={detail.reload.clone()} embedded=true />
                </Panel>
                <LabelsEditor detail={d.clone()} admin={admin} rollout_active={active.is_some()} on_saved={detail.reload.clone()} />
            </div>
            if !d.drift.is_empty() || h.drifted {
                <Panel title="Drift" subtitle="What the latest drift check found different from the applied revision">
                    if d.drift.is_empty() {
                        <p class="muted">{ "The host reported drift; the server did not include the differences." }</p>
                    }
                    { for d.drift.iter().map(|item| html! {
                        <div class="drift">
                            <p class="drift__head">
                                <span class="mono">{ format!("{}/{}[{}]", item.module, item.kind, item.key) }</span>
                                <span>{ crate::components::secret_text(&item.summary) }</span>
                                if let Some(at) = item.detected_at { <span class="muted small">{ relative(at, now) }</span> }
                            </p>
                            if let Some(u) = &item.unified { <DiffView unified={u.clone()} /> }
                        </div>
                    }) }
                </Panel>
            }
            <ModulesPanel detail={d.clone()} on_action={module_actions.clone()} newest={newest.clone()} picked={(*picked).clone()}
                on_pick={{ let picked = picked.clone(); Callback::from(move |run| picked.set(run)) }} />
            <Panel title="Recent runs">{ runs(&d, now, shown, &{
                let picked = picked.clone();
                let newest = newest.clone();
                Callback::from(move |run: String| {
                    picked.set((Some(&run) != newest.as_ref()).then_some(run));
                    if let Some(el) = crate::util::window().document().and_then(|d| d.get_element_by_id("modules")) {
                        el.scroll_into_view();
                    }
                })
            }) }</Panel>
            <Panel title="Facts" subtitle={d.facts_at.map(|at| format!("refreshed {}", relative(at, now))).unwrap_or_default()}>
                if d.facts.is_null() {
                    <Empty title="No facts yet" message="Facts arrive with the agent's first session." />
                } else {
                    <JsonTree value={d.facts.clone()} open={1} />
                }
            </Panel>
            <p class="muted small">
                <RouteLink to={Route::Audit} query={vec![("filter".to_string(), crate::logic::filter::equals("subject", &h.id))]}>
                    { format!("Audit entries for {}", h.id) }
                </RouteLink>
            </p>
            { dialog.unwrap_or_default() }
            { target_actions.dialog }
        </div>
    }
}

/// The agent's version, with the upgrade it was asked for (or that one is
/// available).
fn agent_version(
    running: Option<&str>,
    upgrade: Option<&AgentUpgrade>,
    desired: Option<&str>,
    now: DateTime<Utc>,
) -> Html {
    let version = html! { <span class="mono">{ running.unwrap_or("—").to_string() }</span> };
    let status = match upgrade {
        Some(u) if u.state == UpgradeState::Failed => html! {
            <Pill tone={Tone::Danger} label={format!("upgrade to {} failed", u.version)} title={u.error.clone()} />
        },
        Some(u) => html! {
            <Pill tone={Tone::Active} label={format!("upgrading to {}", u.version)}
                title={format!("Requested by {} {}; installs once the agent is idle.", u.requested_by, relative(u.requested_at, now))} />
        },
        None => match desired {
            Some(d)
                if running
                    .is_some_and(|r| r.trim_start_matches('v') != d.trim_start_matches('v')) =>
            {
                html! { <span class="muted small">{ format!("{d} available") }</span> }
            }
            _ => html! {},
        },
    };
    let error = upgrade
        .filter(|u| u.state == UpgradeState::Failed)
        .and_then(|u| u.error.clone())
        .map(|e| html! { <span class="muted small">{ e }</span> });
    html! { <>{ version }{ " " }{ status }{ for error }</> }
}

/// One check as it reads now, with how it fared in the target's recent
/// rollouts: one mark per rollout, oldest to newest, coloured by its worst
/// reading.
pub(super) fn check_row(c: &HostCheck, now: DateTime<Utc>) -> Html {
    let state = CheckState::of(&c.status, c.required);
    html! {
        <li class="checks__item" data-check={format!("{}/{}", c.module, c.check)} data-state={state.slug()}>
            <Pill tone={check_status_tone(&c.status)} label={c.status.clone()} />
            <span class="mono">{ c.check.clone() }</span>
            <span class="badge">{ c.kind.clone() }</span>
            if !c.required { <span class="muted small">{ "informational" }</span> }
            <span class="checks__reason">{ c.reason.clone().unwrap_or_default() }</span>
            <ol class="check-history" aria-label={format!("{} in recent rollouts, oldest first", c.check)}>
                { for c.history.iter().rev().map(|e| {
                    let worst = CheckState::of(&e.worst, c.required);
                    let label = match &e.rollout_id {
                        Some(id) => format!("rollout {}", rollout_label(id)),
                        None => format!("revision {}", commit(&e.revision)),
                    };
                    let title = format!(
                        "{label}: worst {}, last {} · {} {} from {} to {}",
                        e.worst, e.status, e.readings,
                        if e.readings == 1 { "reading" } else { "readings" },
                        timestamp(e.first_at), timestamp(e.last_at),
                    );
                    html! {
                        <li class={classes!("check-history__epoch", worst.tone().class())}
                            data-state={worst.slug()} title={title.clone()} aria-label={title} />
                    }
                }) }
            </ol>
            <span class="muted small" title={format!("{} · {}", scope_label(&c.scope), timestamp(c.at))}>{ relative(c.at, now) }</span>
        </li>
    }
}

fn scope_label(scope: &str) -> &str {
    match scope {
        "drift" => "Drift check",
        "baseline" => "Baseline, before apply",
        "bake" => "Bake, after apply",
        "rollout" => "Pre-phase re-check",
        other => other,
    }
}

pub(super) fn runs(
    d: &HostDetail,
    now: DateTime<Utc>,
    shown: Option<String>,
    view: &Callback<String>,
) -> Html {
    if d.recent_runs.is_empty() {
        return html! { <Empty title="No runs yet" /> };
    }
    html! {
        <div class="table-wrap">
            <table class="table table--compact">
                <thead><tr>
                    <th scope="col">{ "Started" }</th><th scope="col">{ "Revision" }</th><th scope="col">{ "Plan" }</th>
                    <th scope="col">{ "Outcome" }</th><th scope="col">{ "Changes" }</th><th scope="col">{ "Took" }</th><th scope="col">{ "Trace" }</th>
                    <th scope="col"><span class="visually-hidden">{ "Report" }</span></th>
                </tr></thead>
                <tbody>
                    { for d.recent_runs.iter().map(|r| html! {
                        <tr class={classes!((!r.id.is_empty() && shown.as_deref() == Some(r.id.as_str())).then_some("is-open"))}
                            data-run={(!r.id.is_empty()).then(|| r.id.clone())}>
                            <td title={timestamp(r.started_at)}>{ relative(r.started_at, now) }</td>
                            <td class="mono" title={r.revision.clone()}>{ commit(&r.revision) }</td>
                            if let Some(action) = &r.action {
                                <td><span class="badge" title="An operator's action">{ format!("action {action}") }</span></td>
                            } else {
                                <td class="mono" title={r.plan_digest.clone()}>{ short(&r.plan_digest) }</td>
                            }
                            <td>
                                <Pill tone={outcome_tone(&r.outcome)} label={r.outcome.replace('_', " ")} />
                                if let Some(e) = &r.error { <ErrorBlock raw={e.clone()} tone={outcome_tone(&r.outcome)} compact=true /> }
                            </td>
                            <td><RunChanges changes={r.changes} removals={r.removals} /></td>
                            <td>{ if r.finished_at > r.started_at {
                                html! { <Took label={short_duration(r.finished_at.signed_duration_since(r.started_at).num_seconds())} /> }
                            } else { html! { <span class="muted">{ "—" }</span> } } }</td>
                            <td><TraceLink id={r.trace_id.clone()} url={r.trace_url.clone()} /></td>
                            <td>
                                if !r.id.is_empty() {
                                    <button type="button" class="btn btn--subtle btn--small"
                                        aria-label={format!("Show the report of the run started {}", relative(r.started_at, now))}
                                        onclick={{ let (view, id) = (view.clone(), r.id.clone()); Callback::from(move |_: MouseEvent| view.emit(id.clone())) }}>
                                        { "Report" }
                                    </button>
                                }
                            </td>
                        </tr>
                    }) }
                </tbody>
            </table>
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub(super) struct LabelsProps {
    pub detail: HostDetail,
    pub admin: bool,
    pub rollout_active: bool,
    pub on_saved: Callback<()>,
    /// Shown in place of what the labels would make of the target, which
    /// is then previewed only while there are unsaved changes.
    #[prop_or_default]
    pub aside: Option<Html>,
}

/// Server labels, edited row by row, with what they would make of the host
/// previewed as they are typed.
#[function_component(LabelsEditor)]
pub(super) fn labels_editor(props: &LabelsProps) -> Html {
    let h = &props.detail.summary;
    let toast = use_toast();
    let draft = use_state(|| rows(&h.labels));
    let saving = use_state(|| false);
    let save_error = use_state(|| None::<String>);
    let preview = use_state(|| None::<Result<HostPreview, ApiError>>);

    // A different host, or labels saved elsewhere, reset the draft.
    {
        let draft = draft.clone();
        use_effect_with((h.id.clone(), h.labels.clone()), move |(_, labels)| {
            draft.set(rows(labels));
            || ()
        });
    }

    let parsed = collect(&draft);
    let errors: BTreeMap<usize, String> = parsed
        .as_ref()
        .err()
        .map(|es| es.iter().map(|e| (e.row, e.message.clone())).collect())
        .unwrap_or_default();
    let wanted = parsed.as_ref().ok().cloned();
    let settled = use_debounced(wanted.clone(), 300);

    {
        let (preview, id) = (preview.clone(), h.id.clone());
        use_effect_with(settled.clone(), move |labels| {
            if let Some(labels) = labels.clone() {
                wasm_bindgen_futures::spawn_local(async move {
                    let patch = HostPatch {
                        labels: Some(labels),
                        ..Default::default()
                    };
                    preview.set(Some(api::preview_host(&id, &patch).await));
                });
            }
            || ()
        });
    }

    let changed = wanted
        .as_ref()
        .map(|w| changes(&h.labels, w))
        .unwrap_or_default();
    let edit = |i: usize, key: bool| {
        let draft = draft.clone();
        Callback::from(move |e: InputEvent| {
            let value = e.target_unchecked_into::<HtmlInputElement>().value();
            let mut next = (*draft).clone();
            if let Some(row) = next.get_mut(i) {
                if key {
                    row.key = value
                } else {
                    row.value = value
                }
            }
            draft.set(next);
        })
    };
    let remove = |i: usize| {
        let draft = draft.clone();
        Callback::from(move |_: MouseEvent| {
            let mut next = (*draft).clone();
            next.remove(i);
            draft.set(next);
        })
    };
    let add = {
        let draft = draft.clone();
        Callback::from(move |_: MouseEvent| {
            let mut next = (*draft).clone();
            next.push(Row::default());
            draft.set(next);
        })
    };
    let reset = {
        let (draft, labels) = (draft.clone(), h.labels.clone());
        Callback::from(move |_: MouseEvent| draft.set(rows(&labels)))
    };
    let save = {
        let (saving, save_error, toast, on_saved, id) = (
            saving.clone(),
            save_error.clone(),
            toast.clone(),
            props.on_saved.clone(),
            h.id.clone(),
        );
        let wanted = wanted.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(labels) = wanted.clone() else { return };
            let (saving, save_error, toast, on_saved, id) = (
                saving.clone(),
                save_error.clone(),
                toast.clone(),
                on_saved.clone(),
                id.clone(),
            );
            saving.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::patch_host(
                    &id,
                    &HostPatch {
                        labels: Some(labels),
                        ..Default::default()
                    },
                )
                .await
                {
                    Ok(_) => {
                        save_error.set(None);
                        toast.ok(format!(
                            "Labels of {id} saved; they apply to the next rollout's selection."
                        ));
                        on_saved.emit(());
                    }
                    Err(e) => save_error.set(Some(e.to_string())),
                }
                saving.set(false);
            });
        })
    };

    let preview_view = match &*preview {
        Some(Ok(p)) => html! {
            <div class="preview" data-testid="label-preview" aria-live="polite">
                <div class="preview__row">
                    <span class="preview__label">{ "Phase" }</span>
                    <span class="preview__phase strong" data-testid="preview-phase">{ p.phase.clone().unwrap_or_else(|| "none (not rolled)".into()) }</span>
                </div>
                <div class="preview__row">
                    <span class="preview__label">{ "Groups" }</span>
                    <span data-testid="preview-groups">{ for p.groups.iter().map(|g| html! { <span class="badge">{ g.clone() }</span> }) }</span>
                </div>
                <div class="preview__row">
                    <span class="preview__label">{ "Effective" }</span>
                    <span>{ for p.effective_labels.iter().map(|(k, v)| html! { <span class="label">{ format!("{k}={v}") }</span> }) }</span>
                </div>
            </div>
        },
        // Without the server's answer, the merge can still be shown: which
        // groups and phase it yields is the server's to say.
        other => html! { <>
            if let Some(Err(e)) = other { <Note kind={NoteKind::Warn}>{ format!("No preview of groups and phase: {e}") }</Note> }
            else { <p class="muted small">{ "Previewing…" }</p> }
            if let Some(w) = &wanted {
                <div class="preview__row">
                    <span class="preview__label">{ "Effective" }</span>
                    <span>{ for effective(&h.agent_labels, w).iter().map(|(k, v)| html! { <span class="label">{ format!("{k}={v}") }</span> }) }</span>
                </div>
            }
        </> },
    };

    let preview_block = html! { <>
        if props.aside.is_none() || !changed.is_empty() {
            <h3 class="subhead">{ "Would land in" }</h3>
            { preview_view }
        }
    </> };
    let labels = html! {
            <div class="labels">
                <div class="labels__edit">
                    <h3 class="subhead">{ "Reported by the agent" }</h3>
                    <p>{ for h.agent_labels.iter().map(|(k, v)| html! { <span class="label">{ format!("{k}={v}") }</span> }) }
                        if h.agent_labels.is_empty() { <span class="muted">{ "none" }</span> }</p>
                    <h3 class="subhead">{ "Set by operators" }</h3>
                    if props.admin {
                        <div class="kv-rows" role="group" aria-label="Server labels">
                            { for draft.iter().enumerate().map(|(i, row)| html! {
                                <div class={classes!("kv-row", errors.contains_key(&i).then_some("kv-row--error"))}>
                                    <input class="input mono" type="text" placeholder="key" aria-label={format!("Label {} key", i + 1)}
                                        value={row.key.clone()} oninput={edit(i, true)} spellcheck="false" />
                                    <span aria-hidden="true">{ "=" }</span>
                                    <input class="input mono" type="text" placeholder="value" aria-label={format!("Label {} value", i + 1)}
                                        value={row.value.clone()} oninput={edit(i, false)} spellcheck="false" />
                                    <button type="button" class="icon-btn" aria-label={format!("Remove label {}", if row.key.is_empty() { (i + 1).to_string() } else { row.key.clone() })}
                                        onclick={remove(i)}>{ crate::components::icons::close() }</button>
                                    if let Some(msg) = errors.get(&i) { <span class="kv-row__error" role="alert">{ msg.clone() }</span> }
                                </div>
                            }) }
                            <Button small=true kind={Kind::Subtle} onclick={add}>{ crate::components::icons::plus() }{ "Add label" }</Button>
                        </div>
                    } else {
                        <p>{ for h.labels.iter().map(|(k, v)| html! { <span class="label label--server">{ format!("{k}={v}") }</span> }) }
                            if h.labels.is_empty() { <span class="muted">{ "none" }</span> }</p>
                    }
                </div>
                <div class="labels__preview">
                    { props.aside.clone().unwrap_or_default() }
                    { preview_block }
                    if !changed.is_empty() {
                        <ul class="changes" aria-label="Unsaved changes">
                            { for changed.iter().map(|c| match c {
                                Change::Added { key, value } => html! { <li class="text-ok">{ format!("+ {key}={value}") }</li> },
                                Change::Removed { key, value } => html! { <li class="text-danger">{ format!("− {key}={value}") }</li> },
                                Change::Changed { key, from, to } => html! { <li class="text-warn">{ format!("~ {key}: {from} → {to}") }</li> },
                            }) }
                        </ul>
                    }
                    if props.rollout_active && !changed.is_empty() {
                        <Note>{ "A rollout is in progress: the change applies to the next rollout's selection and does not move this host between phases mid-flight." }</Note>
                    }
                    if let Some(e) = &*save_error { <Note kind={NoteKind::Danger}>{ e.clone() }</Note> }
                </div>
            </div>
    };
    // In the card's head like every other control, once there is something to save.
    let actions = if props.admin && (!changed.is_empty() || wanted.is_none()) {
        html! { <>
            <Button small=true kind={Kind::Subtle} onclick={reset}>{ "Discard" }</Button>
            <Button small=true kind={Kind::Primary} onclick={save} busy={*saving} disabled={wanted.is_none()}>{ "Save labels" }</Button>
        </> }
    } else {
        html! {}
    };
    html! {
        <Panel title="Labels" subtitle="Operator labels win over the agent's own" {actions}>
            { labels }
        </Panel>
    }
}

#[derive(Properties, PartialEq)]
pub(super) struct NotesProps {
    pub id: String,
    pub notes: String,
    pub admin: bool,
    pub on_saved: Callback<()>,
    /// Drawn inside another panel, under a subheading, not as its own.
    #[prop_or_default]
    pub embedded: bool,
}

/// How long typing pauses before notes save themselves.
const NOTES_SAVE_AFTER_MS: u32 = 800;

/// Where the notes stand against the server, said beside the box.
#[derive(Clone, PartialEq)]
enum NotesStatus {
    Idle,
    Saving,
    Saved,
    Failed(String),
}

/// Notes that save themselves: shortly after typing pauses, when the box
/// loses focus, or when the page goes, and only when they changed. A reload
/// never overwrites what is still being typed.
#[function_component(NotesEditor)]
pub(super) fn notes_editor(props: &NotesProps) -> Html {
    let draft = use_state(|| props.notes.clone());
    let status = use_state(|| NotesStatus::Idle);
    // The box's text, what the server holds, whose notes they are and the
    // pending save, read by callbacks that outlive a render.
    let typed = use_mut_ref(|| props.notes.clone());
    let synced = use_mut_ref(|| props.notes.clone());
    let owner = use_mut_ref(|| props.id.clone());
    let timer = use_mut_ref(|| None::<gloo_timers::callback::Timeout>);

    let flush = {
        let (typed, synced, owner, timer, status, on_saved) = (
            typed.clone(),
            synced.clone(),
            owner.clone(),
            timer.clone(),
            status.clone(),
            props.on_saved.clone(),
        );
        Callback::from(move |()| {
            timer.borrow_mut().take();
            let notes = typed.borrow().clone();
            if notes == *synced.borrow() {
                return;
            }
            let (synced, status, on_saved, id) = (
                synced.clone(),
                status.clone(),
                on_saved.clone(),
                owner.borrow().clone(),
            );
            status.set(NotesStatus::Saving);
            wasm_bindgen_futures::spawn_local(async move {
                let patch = HostPatch {
                    notes: Some(notes.clone()),
                    ..Default::default()
                };
                match api::patch_host(&id, &patch).await {
                    Ok(_) => {
                        *synced.borrow_mut() = notes;
                        status.set(NotesStatus::Saved);
                        on_saved.emit(());
                    }
                    Err(e) => status.set(NotesStatus::Failed(e.to_string())),
                }
            });
        })
    };
    // Another target starts afresh; the server's notes replace the box's
    // only while nothing unsaved is in it.
    {
        let (draft, typed, synced, owner, status) = (
            draft.clone(),
            typed.clone(),
            synced.clone(),
            owner.clone(),
            status.clone(),
        );
        use_effect_with(
            (props.id.clone(), props.notes.clone()),
            move |(id, notes)| {
                let moved = *owner.borrow() != *id;
                if moved || *typed.borrow() == *synced.borrow() {
                    *owner.borrow_mut() = id.clone();
                    *typed.borrow_mut() = notes.clone();
                    *synced.borrow_mut() = notes.clone();
                    draft.set(notes.clone());
                    if moved {
                        status.set(NotesStatus::Idle);
                    }
                }
                || ()
            },
        );
    }
    // What is still unsaved is saved on the way out.
    {
        let flush = flush.clone();
        use_effect_with((), move |_| move || flush.emit(()));
    }
    let oninput = {
        let (draft, typed, timer, flush) =
            (draft.clone(), typed.clone(), timer.clone(), flush.clone());
        Callback::from(move |e: InputEvent| {
            let text = e.target_unchecked_into::<HtmlTextAreaElement>().value();
            *typed.borrow_mut() = text.clone();
            draft.set(text);
            let flush = flush.clone();
            *timer.borrow_mut() = Some(gloo_timers::callback::Timeout::new(
                NOTES_SAVE_AFTER_MS,
                move || flush.emit(()),
            ));
        })
    };
    let onblur = flush.reform(|_: FocusEvent| ());
    let unsaved = *draft != *synced.borrow();
    let note = match (&*status, unsaved) {
        (NotesStatus::Saving, _) => html! { <span class="muted small">{ "Saving…" }</span> },
        (NotesStatus::Failed(e), _) => {
            html! { <span class="text-danger small">{ format!("Not saved: {e}") }</span> }
        }
        (_, true) => html! { <span class="muted small">{ "Unsaved" }</span> },
        (NotesStatus::Saved, false) => html! { <span class="muted small">{ "Saved" }</span> },
        (NotesStatus::Idle, false) => html! {},
    };
    let body = html! { <>
            if props.admin {
                <textarea class="input notes" rows="4" value={(*draft).clone()} {oninput} {onblur} aria-label="Notes"
                    placeholder="Anything the next operator should know; saved as you type." />
                <p class="notes-status" data-testid="notes-status" aria-live="polite">{ note }</p>
            } else if props.notes.is_empty() {
                <p class="muted">{ "No notes." }</p>
            } else {
                <p class="notes-text">{ props.notes.clone() }</p>
            }
    </> };
    if props.embedded {
        return html! { <section class="panel__section" aria-label="Notes">
            <h3 class="subhead">{ "Notes" }</h3>
            { body }
        </section> };
    }
    html! { <Panel title="Notes">{ body }</Panel> }
}
