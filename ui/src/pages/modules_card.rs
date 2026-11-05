//! The card both target pages share: what Buckle manages there, workload by
//! workload on a cluster and module by module on a host, each row with how
//! it reads now and what a run did to it, and opened, its checks, that
//! run's steps and its resources.

use buckle_api::{
    ActionInfo, DriftItem, HostCheck, HostDetail, ModuleReport, ModuleState, RemovalReport,
    ResourceSummary, RunReport, RunSummary,
};
use web_sys::HtmlSelectElement;
use yew::prelude::*;

use super::host::check_row;
use crate::api;
use crate::components::blueprint::resource_steps;
use crate::components::changes::Took;
use crate::components::run_report::{health_bar, module_steps, removal_steps, run_summary};
use crate::components::{
    ActionsButton, Button, Changes, Empty, ErrorBlock, ErrorState, Kind, Loading, Panel, Pill,
    icons,
};
use crate::hooks::{RunSource, use_fetch, use_now, use_run_report};
use crate::logic::actions::of_module;
use crate::logic::blueprint::blueprint;
use crate::logic::clusters::{WorkloadRow, WorkloadState, module_rows, workload_rows};
use crate::logic::format::{commit, count, millis, relative, timestamp};
use crate::logic::run::{
    elapsed_ms, expanded_by_default, follow_run_list, module_label, module_slug, module_tally,
    module_tone, resource_module, run_finished, run_label,
};
use crate::logic::tone::Tone;
use crate::store::use_live;

#[derive(Properties, PartialEq)]
pub(super) struct ModulesProps {
    pub detail: HostDetail,
    /// A cluster's workloads, else a host's modules.
    #[prop_or_default]
    pub cluster: bool,
    /// Set where the operator may run the workloads' actions.
    pub on_action: Option<Callback<ActionInfo>>,
    /// The newest run heard of (a run may have started since the detail).
    pub newest: Option<String>,
    /// The run the reader picked; `None` follows the newest.
    pub picked: Option<String>,
    pub on_pick: Callback<Option<String>>,
}

/// What Buckle manages on a target, workload by workload on a cluster and
/// module by module on a host: how each reads now, what the run shown did
/// to it, a cluster's jobs linked into its UI. A row opens (rendered only
/// once opened) to its checks, that run's steps and its resources.
#[function_component(ModulesPanel)]
pub(super) fn modules_panel(props: &ModulesProps) -> Html {
    let cluster = props.cluster;
    let (one, many) = if cluster {
        ("workload", "workloads")
    } else {
        ("module", "modules")
    };
    let d = &props.detail;
    let shown = props.picked.clone().or_else(|| props.newest.clone());
    let older = props.picked.is_some() && props.picked != props.newest;
    let feed = use_run_report(
        shown.clone().map(|run| RunSource::Host {
            host: d.summary.id.clone(),
            run,
        }),
        d.latest_run.clone(),
    );
    let run = feed.report.as_ref();
    let live = run.is_some_and(|r| !run_finished(r.state));
    let now = use_now(if live { 1 } else { 30 });
    // What the reader opened or closed; the rest follows the defaults (a
    // problem, drift or the module being applied opens), reset with each run shown.
    let overrides = use_state_eq(Vec::<(String, bool)>::new);
    {
        let overrides = overrides.clone();
        use_effect_with(shown.clone(), move |_| {
            overrides.set(Vec::new());
            || ()
        });
    }
    let rows = if cluster {
        workload_rows(d, d.latest_run.as_ref())
    } else {
        module_rows(d, run.or(d.latest_run.as_ref()))
    };
    let failing = rows
        .iter()
        .filter(|r| r.state == WorkloadState::Failing)
        .count();
    let deploying = rows
        .iter()
        .filter(|r| r.state == WorkloadState::Deploying)
        .count();
    let mut subtitle = format!(
        "What Buckle manages on this {}, how it reads and what {} did: {}",
        if cluster { "cluster" } else { "host" },
        if older {
            "the run shown"
        } else {
            "the latest run"
        },
        count(rows.len(), one, many)
    );
    if failing > 0 {
        subtitle.push_str(&format!(" · {failing} failing"));
    }
    if deploying > 0 {
        subtitle.push_str(&format!(
            " · {deploying} {}",
            if cluster { "deploying" } else { "applying" }
        ));
    }
    let head = match (&shown, run, &feed.error) {
        (None, _, _) => html! {},
        (Some(_), Some(r), _) => html! {
            <div class="run workloads__run" data-run={r.id.clone()} data-state={run_label(r.state).replace(' ', "-")}>
                { run_summary(r, now, many) }
                <ErrorBlock error={r.error.clone()} context={AttrValue::from("The run")} />
            </div>
        },
        (Some(_), None, Some(e)) => {
            html! { <ErrorState error={e.clone()} retry={feed.reload.clone()} /> }
        }
        (Some(_), None, None) => html! { <Loading label="Loading the run…" /> },
    };
    let back = older.then(|| {
        let on_pick = props.on_pick.clone();
        html! { <Button small=true kind={Kind::Subtle} onclick={Callback::from(move |_| on_pick.emit(None))}>{ "Show the newest" }</Button> }
    });
    let picker = html! {
        <RunPicker host={d.summary.id.clone()} shown={shown.clone()} newest={props.newest.clone()} on_pick={props.on_pick.clone()} />
    };
    let removals: Vec<&RemovalReport> =
        run.map(|r| r.removals.iter().collect()).unwrap_or_default();
    let body = if rows.is_empty() {
        if cluster {
            html! { <Empty title="No workloads" message="The revision selects no workloads for this cluster. Add workloads/NAME/workload.yaml whose `when` matches it." /> }
        } else {
            html! { <Empty title="Nothing managed yet" message="Modules appear once a revision has been applied." /> }
        }
    } else {
        html! {
            <div class="table-wrap">
                <table class="table table--workloads" data-testid={many}>
                    <thead><tr>
                        <th scope="col">{ if cluster { "Workload" } else { "Module" } }</th>
                        <th scope="col">{ "Health" }</th>
                        <th scope="col">{ if older { "This run" } else { "Latest run" } }</th>
                        <th scope="col">{ if cluster { "Jobs" } else { "Resources" } }</th>
                        <th scope="col">{ "Applied" }</th>
                        <th scope="col"><span class="visually-hidden">{ "Details" }</span></th>
                    </tr></thead>
                    <tbody>
                        { for rows.iter().map(|r| {
                            let module = run.and_then(|x| x.modules.iter().find(|m| m.name == r.name));
                            let is_open = overrides
                                .iter()
                                .find(|(n, _)| *n == r.name)
                                .map(|(_, o)| *o)
                                .unwrap_or_else(|| {
                                    r.state == WorkloadState::Failing
                                        || module.is_some_and(expanded_by_default)
                                        || d.drift.iter().any(|x| x.module == r.name)
                                });
                            let toggle = {
                                let (overrides, name) = (overrides.clone(), r.name.clone());
                                Callback::from(move |e: MouseEvent| {
                                    let own = e
                                        .target_dyn_into::<web_sys::Element>()
                                        .and_then(|t| t.closest("a").ok().flatten())
                                        .is_some();
                                    if !own {
                                        let mut next: Vec<(String, bool)> =
                                            (*overrides).iter().filter(|(n, _)| *n != name).cloned().collect();
                                        next.push((name.clone(), !is_open));
                                        overrides.set(next);
                                    }
                                })
                            };
                            let gone: Vec<&RemovalReport> = removals
                                .iter()
                                .copied()
                                .filter(|x| resource_module(&x.resource) == Some(r.name.as_str()))
                                .collect();
                            html! { <>
                                { workload_row(d, r, cluster, module, run, is_open, toggle, now, live, props.on_action.as_ref().map(|on_run| html! {
                                    <ActionsButton actions={of_module(&d.actions, &r.name)} on_run={on_run.clone()}
                                        testid={format!("actions-{}", r.name)} />
                                })) }
                                if is_open {
                                    <tr class="table__detail" key={format!("{}-detail", r.name)} data-detail-of={r.name.clone()}>
                                        <td colspan="6">
                                            { workload_detail(d, r, cluster, module, &gone, run, now, live) }
                                        </td>
                                    </tr>
                                }
                            </> }
                        }) }
                    </tbody>
                </table>
            </div>
        }
    };
    html! {
        <Panel title={if cluster { "Workloads" } else { "Modules" }} id={many} {subtitle} actions={html! { <>{ back.unwrap_or_default() }{ picker }</> }}>
            { head }
            { body }
        </Panel>
    }
}

/// An opened workload: its checks with their history, what the run shown
/// did to it, and the resources Buckle manages for it.
#[allow(clippy::too_many_arguments)]
fn workload_detail(
    d: &HostDetail,
    r: &WorkloadRow,
    cluster: bool,
    module: Option<&ModuleReport>,
    removals: &[&RemovalReport],
    run: Option<&RunReport>,
    now: chrono::DateTime<chrono::Utc>,
    live: bool,
) -> Html {
    let checks: Vec<&HostCheck> = d
        .health_checks
        .iter()
        .filter(|c| c.module == r.name)
        .collect();
    let resources: Vec<ResourceSummary> = d
        .resources
        .iter()
        .filter(|x| x.module == r.name)
        .cloned()
        .collect();
    let drift: Vec<DriftItem> = d
        .drift
        .iter()
        .filter(|x| x.module == r.name)
        .cloned()
        .collect();
    html! {
        <div class="workload-detail" data-testid="workload-detail">
            <section aria-label={format!("Health of {}", r.name)}>
                <h4 class="subhead">{ "Health" }</h4>
                if checks.is_empty() {
                    <p class="muted small">{ if cluster {
                        "No checks read yet: its jobs are checked during rollouts and whenever the cluster's health is read."
                    } else {
                        "It declares no health checks, or none has been read yet."
                    } }</p>
                } else {
                    <ul class="checks">{ for checks.into_iter().map(|c| check_row(c, now)) }</ul>
                }
            </section>
            <section aria-label={format!("{} in the run", r.name)}>
                <h4 class="subhead">
                    { "In the run" }
                    if let Some(m) = module {
                        { " " }<Pill tone={module_tone(m.state)} label={module_label(m.state)} />
                    }
                </h4>
                { match (module, removals.is_empty()) {
                    (Some(m), _) => module_steps(m, now, live),
                    (None, false) => html! {},
                    (None, true) => html! { <p class="muted small">{ if run.is_some() { "Not part of this run." } else { "No run yet." } }</p> },
                } }
                if !removals.is_empty() {
                    <h5 class="run-module__sub">{ "Removed" }</h5>
                    { removal_steps(removals, now, live) }
                }
            </section>
            <section aria-label={format!("Resources of {}", r.name)}>
                <h4 class="subhead">{ "Blueprint" }<span class="muted">{ format!(" · {}", count(resources.len(), "resource", "resources")) }</span></h4>
                if resources.is_empty() {
                    <p class="muted small">{ "Nothing applied yet." }</p>
                } else {
                    { resource_steps(&resources, &drift, run, now) }
                }
            </section>
        </div>
    }
}

fn state_pill(r: &WorkloadRow, cluster: bool) -> Html {
    match r.state {
        WorkloadState::Failing => {
            html! { <Pill tone={Tone::Danger} label="failing" title={r.reason.clone()} /> }
        }
        WorkloadState::Deploying => {
            html! { <Pill tone={Tone::Active} label={if cluster { "deploying" } else { "applying" }} /> }
        }
        // The norm, said quietly.
        WorkloadState::Running if r.checks.1 > 0 => {
            html! { <span class="muted">{ "healthy" }</span> }
        }
        WorkloadState::Running => html! { <span class="muted">{ "running" }</span> },
        WorkloadState::NotApplied => html! { <span class="muted">{ "not applied yet" }</span> },
    }
}

#[allow(clippy::too_many_arguments)]
fn workload_row(
    d: &HostDetail,
    r: &WorkloadRow,
    cluster: bool,
    module: Option<&ModuleReport>,
    run: Option<&RunReport>,
    open: bool,
    toggle: Callback<MouseEvent>,
    now: chrono::DateTime<chrono::Utc>,
    live: bool,
    actions: Option<Html>,
) -> Html {
    html! {
        <tr key={r.name.clone()} class={classes!("table__toggle-row", open.then_some("is-open"))}
            data-workload={cluster.then(|| r.name.clone())} data-module={(!cluster).then(|| r.name.clone())}
            data-state={r.state.label().replace(' ', "-")} onclick={toggle.clone()}>
            <td>
                <span class="mono strong">{ r.name.clone() }</span>
                if !r.description.is_empty() { <span class="cell-sub">{ r.description.clone() }</span> }
                if !r.needs.is_empty() { <span class="cell-sub">{ format!("needs {}", r.needs.join(", ")) }</span> }
            </td>
            <td>
                { state_pill(r, cluster) }
                if let (WorkloadState::Failing, Some(reason)) = (r.state, &r.reason) {
                    <span class="cell-sub">{ reason.clone() }</span>
                }
                if r.checks.1 > 0 && r.state != WorkloadState::Failing {
                    <span class="cell-sub muted" data-testid="workload-checks">{ format!("{} of {} passing", r.checks.0, count(r.checks.1, "check", "checks")) }</span>
                }
            </td>
            <td data-testid="workload-run" data-run-state={module.map(|m| module_slug(m.state))}>
                { match module {
                    Some(m) => html! { <>
                        <Pill tone={module_tone(m.state)} label={module_label(m.state)} />
                        { " " }<Changes tally={module_tally(m)} />
                        { " " }{ health_bar(m, run.and_then(|x| x.started_at)) }
                        if let Some(took) = elapsed_ms(m.duration_ms, m.started_at, m.finished_at, now, live && m.state == ModuleState::Running).map(millis) {
                            { " " }<Took label={took} running={live && m.state == ModuleState::Running} />
                        }
                    </> },
                    None => html! { <span class="muted">{ "—" }</span> },
                } }
                if let Some(p) = &r.progress {
                    <span class="cell-sub mono" data-testid="workload-progress">{ p.clone() }</span>
                }
            </td>
            if !cluster {
                <td>{ resources_cell(d, &r.name, run) }</td>
            } else {
            <td>
                { for r.jobs.iter().map(|j| html! {
                    <span class="workload-job">
                        <span class="mono">{ j.key.clone() }</span>
                        if let Some(url) = &j.url {
                            { " " }
                            <a href={url.clone()} target="_blank" rel="noopener noreferrer"
                                aria-label={format!("Open {} in the cluster's UI", j.key)}>{ "Nomad" }{ icons::external() }</a>
                        }
                    </span>
                }) }
                if r.jobs.is_empty() { <span class="muted">{ "—" }</span> }
                if !r.kinds.is_empty() { <span class="cell-sub muted">{ r.kinds.join(", ") }</span> }
            </td>
            }
            <td class="nowrap">
                { match &r.applied {
                    Some((at, rev)) => html! { <>
                        <span title={timestamp(*at)}>{ relative(*at, now) }</span>
                        <span class="cell-sub mono" title={rev.clone()}>{ commit(rev) }</span>
                    </> },
                    None => html! { <span class="muted">{ "—" }</span> },
                } }
            </td>
            <td class="row-controls">
                // Its actions are its own, not the row's toggle.
                <span class="row-controls__actions" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                    { actions.unwrap_or_default() }
                </span>
                <button type="button" class="icon-btn row-toggle" onclick={toggle}
                    aria-expanded={open.to_string()} aria-label={format!("Details of {}", r.name)}
                    title={if open { "Hide details" } else { "Show health, run and resources" }}>
                    { icons::chevron() }
                </button>
            </td>
        </tr>
    }
}

/// A host module's resources at a glance: how many, how many removal
/// cleans up, and their worst state where it wants a look.
fn resources_cell(d: &HostDetail, module: &str, run: Option<&RunReport>) -> Html {
    let own: Vec<ResourceSummary> = d
        .resources
        .iter()
        .filter(|x| x.module == module)
        .cloned()
        .collect();
    let drift: Vec<DriftItem> = d
        .drift
        .iter()
        .filter(|x| x.module == module)
        .cloned()
        .collect();
    let Some(m) = blueprint(&own, &drift, run)
        .into_iter()
        .find(|m| m.name == module)
    else {
        return html! { <span class="muted">{ "—" }</span> };
    };
    let state = m.state();
    html! { <>
        { count(m.resources.len(), "resource", "resources") }
        if state.is_problem() {
            { " " }<Pill tone={state.tone()} label={state.label()} />
        }
        <span class="cell-sub muted" title="Deleted from the host if the revision drops them">{ format!("{} tagged for cleanup", m.cleanup()) }</span>
    </> }
}

#[derive(Properties, PartialEq)]
struct PickerProps {
    host: String,
    shown: Option<String>,
    newest: Option<String>,
    on_pick: Callback<Option<String>>,
}

/// Which run the card shows: the target's runs, newest first, their states
/// kept current and the list fetched again when a run it lacks starts.
#[function_component(RunPicker)]
fn run_picker(props: &PickerProps) -> Html {
    let now = use_now(30);
    let shown = &props.shown;
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
                { if runs.is_empty() { "The runs could not be listed." } else { "The list of runs may be out of date." } }
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
                    if let (false, Some(s)) = (listed, shown) {
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
    html! { <>{ list_error.unwrap_or_default() }{ picker.unwrap_or_default() }</> }
}
