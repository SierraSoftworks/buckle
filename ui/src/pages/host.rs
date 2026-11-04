//! Host detail (design §12): the facts tree, the host's blueprint (what
//! Buckle manages there, module by module), each health check's current
//! reading and its worst in recent rollouts, drift diffs, the latest run
//! module by module and step by step (live while it applies, with any older
//! run a pick away), and, for admins, the controls: server labels with a live preview of the
//! groups and phase they produce, registration approval, pause and resume,
//! agent upgrades, notes, and forget.

use std::collections::BTreeMap;

use buckle_api::{
    AgentUpgrade, Connection, HostCheck, HostDetail, HostPatch, HostPreview, HostStatus, RunReport,
    RunSummary, UpgradeState,
};
use chrono::{DateTime, Utc};
use web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api::{self, ApiError};
use crate::app::{Route, use_auth};
use crate::components::{
    BlueprintView, Button, Confirm, DiffView, Empty, ErrorBlock, ErrorState, JsonTree, Kind,
    Loading, PageHead, Panel, Pill, Refresh, RouteLink, RunReportView, TraceLink,
    changes::{RunChanges, Took},
    host_state_pills, use_toast,
};
use crate::hooks::{RunSource, use_debounced, use_fetch, use_live_value, use_now, use_run_report};
use crate::logic::format::{
    commit, count, relative, relative_opt, rollout_label, short, short_duration, timestamp,
};
use crate::logic::health::CheckState;
use crate::logic::labels::{Change, Row, changes, collect, effective, rows};
use crate::logic::live::{apply_fleet, apply_host_detail};
use crate::logic::run::{follow_run_list, newest_run, run_label};
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

    let refresh = html! { <Refresh loading={detail.loading} reload={detail.reload.clone()} /> };
    let actions = if admin {
        html! { <>
            if h.status == HostStatus::Pending {
                <Button kind={Kind::Primary} onclick={open(Ask::Approve)}>{ "Approve registration…" }</Button>
            }
            if h.status == HostStatus::Paused {
                <Button onclick={open(Ask::Resume)}>{ "Resume" }</Button>
            } else {
                <Button onclick={open(Ask::Pause)}>{ "Pause…" }</Button>
            }
            if h.connection == Connection::Online {
                <Button onclick={open(Ask::Upgrade)}>{ "Upgrade agent…" }</Button>
            }
            <Button kind={Kind::Danger} onclick={open(Ask::Forget)}>{ "Forget…" }</Button>
        </> }
    } else {
        html! {}
    };
    let actions = html! { <>{ refresh }{ actions }</> };

    let offline = h.connection == Connection::Offline;
    let subtitle = html! {
        <div class="rollout-meta">
            { host_state_pills(h) }
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

    html! {
        <div class="page page--wide">
            <PageHead title={h.id.clone()} {subtitle} {actions} />
            <div class="grid grid--host">
                <Panel title="Summary">
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
                </Panel>
                <NotesEditor id={h.id.clone()} notes={h.notes.clone()} admin={admin} on_saved={detail.reload.clone()} />
                <LabelsEditor detail={d.clone()} admin={admin} rollout_active={active.is_some()} on_saved={detail.reload.clone()} />
            </div>
            { health_panel(&d.health_checks, now) }
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
            <RunsPanel host={h.id.clone()} latest={d.latest_run.clone()} newest={newest.clone()}
                picked={(*picked).clone()} on_pick={{ let picked = picked.clone(); Callback::from(move |run| picked.set(run)) }} />
            <Panel title="Blueprint" subtitle="What Buckle manages on this host, module by module: each resource's state and whether removal cleans it up">
                <BlueprintView resources={d.resources.clone()} drift={d.drift.clone()} run={d.latest_run.clone()} />
            </Panel>
            <Panel title="Recent runs">{ runs(&d, now, (*picked).clone().or(newest.clone()), &{
                let picked = picked.clone();
                let newest = newest.clone();
                Callback::from(move |run: String| {
                    picked.set((Some(&run) != newest.as_ref()).then_some(run));
                    if let Some(el) = crate::util::window().document().and_then(|d| d.get_element_by_id("run-report")) {
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

/// Each check as it reads now, module by module, with how it fared in the
/// host's recent rollouts: one mark per rollout, oldest to newest, coloured
/// by its worst reading.
fn health_panel(checks: &[HostCheck], now: DateTime<Utc>) -> Html {
    let mut by_module: BTreeMap<&str, Vec<&HostCheck>> = BTreeMap::new();
    for c in checks {
        by_module.entry(c.module.as_str()).or_default().push(c);
    }
    html! {
        <Panel title="Health" subtitle="Each check as it reads now, and its worst reading in each recent rollout">
            if checks.is_empty() {
                <Empty title="No health results" message="Checks run with each drift check and during rollouts." />
            }
            { for by_module.into_iter().map(|(module, list)| {
                let worst = list.iter().map(|c| CheckState::of(&c.status, c.required)).max();
                let n = list.len();
                html! {
                    <section class={classes!("health-module", worst.map(|w| w.tone().class()))} data-module={module.to_string()}>
                        <h3 class="health-module__head">
                            <span class="mono">{ module.to_string() }</span>
                            if let Some(w) = worst { <Pill tone={w.tone()} label={w.label()} /> }
                            <span class="muted small">{ count(n, "check", "checks") }</span>
                        </h3>
                        <ul class="checks">{ for list.into_iter().map(|c| check_row(c, now)) }</ul>
                    </section>
                }
            }) }
        </Panel>
    }
}

fn check_row(c: &HostCheck, now: DateTime<Utc>) -> Html {
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

fn runs(
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
                            <td class="mono" title={r.plan_digest.clone()}>{ short(&r.plan_digest) }</td>
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
struct RunsProps {
    host: String,
    /// The detail's latest run, kept current.
    latest: Option<RunReport>,
    /// The newest run heard of (a run may have started since the detail).
    newest: Option<String>,
    /// The run the reader picked; `None` follows the newest.
    picked: Option<String>,
    on_pick: Callback<Option<String>>,
}

/// The report of the host's newest run, live, or of an older one picked
/// from the host's runs.
#[function_component(RunsPanel)]
fn runs_panel(props: &RunsProps) -> Html {
    let now = use_now(30);
    let shown = props.picked.clone().or_else(|| props.newest.clone());
    let older = props.picked.is_some() && props.picked != props.newest;
    let feed = use_run_report(
        shown.clone().map(|run| RunSource::Host {
            host: props.host.clone(),
            run,
        }),
        props.latest.clone(),
    );

    // The picker's list, with states kept current, fetched again when a run
    // it does not list starts.
    let list = use_fetch(
        props.host.clone(),
        |id| async move { api::host_runs(&id).await },
    );
    let store = use_live();
    let mut runs: Vec<RunSummary> = list.value().cloned().unwrap_or_default();
    let unlisted = follow_run_list(
        &mut runs,
        &props.host,
        list.data.as_ref().map(|d| d.0),
        &store.log,
    );
    // Fetched again once per update about a run it lacks: a fetch that
    // fails is tried again on the next such update, not in a loop.
    let tried = use_mut_ref(|| 0u64);
    {
        let (reload, tried) = (list.reload.clone(), tried.clone());
        use_effect_with((unlisted, list.loading), move |(unlisted, loading)| {
            if let Some(seq) = *unlisted
                && !*loading
                && seq > *tried.borrow()
            {
                *tried.borrow_mut() = seq;
                reload.emit(());
            }
            || ()
        });
    }
    runs.retain(|r| !r.id.is_empty());
    let list_error = list.error.as_ref().map(|e| {
        let reload = list.reload.clone();
        html! {
            <span class="run-picker__error" role="alert" title={e.to_string()}>
                { if runs.is_empty() { "The host's runs could not be listed." } else { "The list of runs may be out of date." } }
                <Button small=true kind={Kind::Subtle} onclick={Callback::from(move |_| reload.emit(()))}>{ "Try again" }</Button>
            </span>
        }
    });

    // The select shows the run shown, also after "Show the newest" or a
    // recent-runs row changed it (an option's `selected` attribute does not
    // move a select the reader has already changed).
    let select = use_node_ref();
    {
        let select = select.clone();
        let value = shown.clone().unwrap_or_default();
        use_effect_with((value, runs.len()), move |(value, _)| {
            if let Some(el) = select.cast::<HtmlSelectElement>() {
                el.set_value(value);
            }
            || ()
        });
    }

    let onchange = {
        let (on_pick, newest) = (props.on_pick.clone(), props.newest.clone());
        Callback::from(move |e: Event| {
            let run = e.target_unchecked_into::<HtmlSelectElement>().value();
            on_pick.emit((Some(&run) != newest.as_ref()).then_some(run));
        })
    };
    let picker = (!runs.is_empty()).then(|| {
        let listed = shown.as_ref().is_some_and(|s| runs.iter().any(|r| &r.id == s));
        html! {
            <label class="run-picker">
                <span class="muted small">{ "Run" }</span>
                <select class="input" aria-label="Run to show" ref={select.clone()} {onchange}>
                    if let (false, Some(s)) = (listed, &shown) {
                        <option value={s.clone()} selected=true>{ "newest" }</option>
                    }
                    { for runs.iter().enumerate().map(|(i, r)| {
                        let state = r.state.map(run_label).map(str::to_string).unwrap_or_else(|| r.outcome.replace('_', " "));
                        let label = format!("{}{} · {state} · {}", if i == 0 { "newest · " } else { "" }, relative(r.started_at, now), commit(&r.revision));
                        html! { <option value={r.id.clone()} selected={shown.as_deref() == Some(r.id.as_str())}>{ label }</option> }
                    }) }
                </select>
            </label>
        }
    });
    let back = older.then(|| {
        let on_pick = props.on_pick.clone();
        html! { <Button small=true kind={Kind::Subtle} onclick={Callback::from(move |_| on_pick.emit(None))}>{ "Show the newest" }</Button> }
    });

    let body = match (&shown, &feed.report, &feed.error) {
        (None, _, _) => html! {
            <Empty title="No runs yet" message="A report appears here once the host applies a revision: every module and step, live while it runs." />
        },
        (Some(_), Some(report), _) => html! { <RunReportView report={report.clone()} /> },
        (Some(_), None, Some(e)) => {
            html! { <ErrorState error={e.clone()} retry={feed.reload.clone()} /> }
        }
        (Some(_), None, None) if feed.settled => html! { <Empty title="No report for this run" /> },
        (Some(_), None, None) => html! { <Loading label="Loading the run…" /> },
    };
    html! {
        <Panel title={if older { "Run report" } else { "Latest run" }} id="run-report" class="run-panel"
            subtitle={if older { "An older run of this host, module by module" } else { "The host's newest run, module by module and step by step" }}
            actions={html! { <>{ back.unwrap_or_default() }{ list_error.unwrap_or_default() }{ picker.unwrap_or_default() }</> }}>
            { body }
        </Panel>
    }
}

#[derive(Properties, PartialEq)]
struct LabelsProps {
    detail: HostDetail,
    admin: bool,
    rollout_active: bool,
    on_saved: Callback<()>,
}

/// Server labels, edited row by row, with what they would make of the host
/// previewed as they are typed.
#[function_component(LabelsEditor)]
fn labels_editor(props: &LabelsProps) -> Html {
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
            if let Some(Err(e)) = other { <p class="note note--warn">{ format!("No preview of groups and phase: {e}") }</p> }
            else { <p class="muted small">{ "Previewing…" }</p> }
            if let Some(w) = &wanted {
                <div class="preview__row">
                    <span class="preview__label">{ "Effective" }</span>
                    <span>{ for effective(&h.agent_labels, w).iter().map(|(k, v)| html! { <span class="label">{ format!("{k}={v}") }</span> }) }</span>
                </div>
            }
        </> },
    };

    html! {
        <Panel title="Labels" subtitle="Operator labels win over the agent's own" class="grid__wide">
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
                    <h3 class="subhead">{ "Would land in" }</h3>
                    { preview_view }
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
                        <p class="note">{ "A rollout is in progress: the change applies to the next rollout's selection and does not move this host between phases mid-flight." }</p>
                    }
                    if let Some(e) = &*save_error { <p class="note note--danger" role="alert">{ e.clone() }</p> }
                    if props.admin {
                        <div class="row-actions">
                            <Button kind={Kind::Primary} onclick={save} busy={*saving} disabled={changed.is_empty() || wanted.is_none()}>{ "Save labels" }</Button>
                            <Button kind={Kind::Subtle} onclick={reset} disabled={changed.is_empty()}>{ "Discard" }</Button>
                        </div>
                    }
                </div>
            </div>
        </Panel>
    }
}

#[derive(Properties, PartialEq)]
struct NotesProps {
    id: String,
    notes: String,
    admin: bool,
    on_saved: Callback<()>,
}

#[function_component(NotesEditor)]
fn notes_editor(props: &NotesProps) -> Html {
    let toast = use_toast();
    let draft = use_state(|| props.notes.clone());
    let saving = use_state(|| false);
    {
        let draft = draft.clone();
        use_effect_with(props.notes.clone(), move |n| {
            draft.set(n.clone());
            || ()
        });
    }
    let oninput = {
        let draft = draft.clone();
        Callback::from(move |e: InputEvent| {
            draft.set(e.target_unchecked_into::<HtmlTextAreaElement>().value())
        })
    };
    let save = {
        let (draft, saving, toast, on_saved, id) = (
            draft.clone(),
            saving.clone(),
            toast.clone(),
            props.on_saved.clone(),
            props.id.clone(),
        );
        Callback::from(move |_: MouseEvent| {
            let (notes, saving, toast, on_saved, id) = (
                (*draft).clone(),
                saving.clone(),
                toast.clone(),
                on_saved.clone(),
                id.clone(),
            );
            saving.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::patch_host(
                    &id,
                    &HostPatch {
                        notes: Some(notes),
                        ..Default::default()
                    },
                )
                .await
                {
                    Ok(_) => {
                        toast.ok("Notes saved.");
                        on_saved.emit(());
                    }
                    Err(e) => toast.error(format!("Notes not saved: {e}")),
                }
                saving.set(false);
            });
        })
    };
    html! {
        <Panel title="Notes">
            if props.admin {
                <textarea class="input notes" rows="4" value={(*draft).clone()} {oninput} aria-label="Notes"
                    placeholder="Anything the next operator should know about this host." />
                <div class="row-actions">
                    <Button small=true onclick={save} busy={*saving} disabled={*draft == props.notes}>{ "Save notes" }</Button>
                </div>
            } else if props.notes.is_empty() {
                <p class="muted">{ "No notes." }</p>
            } else {
                <p class="notes-text">{ props.notes.clone() }</p>
            }
        </Panel>
    }
}
