//! The card both target pages share: what Buckle manages there, workload by
//! workload on a cluster and module by module on a host, each row with how
//! it reads now and what a run did to it, and opened, its checks, that
//! run's steps and its resources.

use buckle_api::{
    ActionInfo, DriftItem, HostCheck, HostDetail, ModuleReport, ModuleState, RemovalReport,
    ResourceSummary, RunReport, RunSummary,
};
use yew::prelude::*;

use super::host::check_row;
use crate::api;
use crate::app::Route;
use crate::components::RouteLink;
use crate::components::blueprint::resource_steps;
use crate::components::changes::Took;
use crate::components::run_report::{health_bar, module_steps, removal_steps, run_summary};
use crate::components::{
    ActionsButton, Button, Changes, Empty, ErrorBlock, ErrorState, Kind, Loading, Panel, Pill,
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
    /// Set where the operator may check a workload's placement for drift
    /// (named by the click).
    #[prop_or_default]
    pub on_check_drift: Option<Callback<String>>,
    /// The newest run heard of (a run may have started since the detail).
    pub newest: Option<String>,
    /// The run the reader picked; `None` follows the newest.
    pub picked: Option<String>,
    pub on_pick: Callback<Option<String>>,
}

/// What Buckle manages on a target, workload by workload on a cluster and
/// module by module on a host: how each reads now, what the run shown did
/// to it and its resources at a glance. A row opens (rendered only once
/// opened) to its checks, that run's steps and its resources, a cluster's
/// jobs linked into its UI.
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
            <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true class="table--workloads" attrs={yewi::attrs! { "data-testid" => many }}>
                    <thead><tr>
                        <th scope="col">{ if cluster { "Workload" } else { "Module" } }</th>
                        <th scope="col">{ "Health" }</th>
                        <th scope="col">{ if older { "This run" } else { "Latest run" } }</th>
                        <th scope="col">{ "Resources" }</th>
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
                                { workload_row(d, r, cluster, false, module, run, is_open, toggle, now, live,
                                    row_actions(d, &r.name, props.on_action.as_ref(), props.on_check_drift.as_ref().map(|c| { let n = r.name.clone(); c.reform(move |_| n.clone()) }), format!("actions-{}", r.name))) }
                                if is_open {
                                    <tr class="yi-table__detail" key={format!("{}-detail", r.name)} data-detail-of={r.name.clone()}>
                                        <td colspan="6">
                                            { workload_detail(d, r, cluster, module, &gone, run, now, live) }
                                        </td>
                                    </tr>
                                }
                            </> }
                        }) }
                    </tbody>
            </yewi::SimpleTable>
        }
    };
    html! {
        <Panel title={if cluster { "Workloads" } else { "Modules" }} id={many}
            {subtitle} actions={html! { <>{ back.unwrap_or_default() }{ picker }</> }}>
            { head }
            { body }
        </Panel>
    }
}

#[derive(Properties, PartialEq)]
pub(super) struct PlacementProps {
    /// The cluster.
    pub detail: HostDetail,
    pub workload: String,
    /// Set where the operator may run the workload's actions there.
    pub on_action: Option<Callback<ActionInfo>>,
    /// Set where the operator may check the placement for drift.
    #[prop_or_default]
    pub on_check_drift: Option<Callback<()>>,
    /// Opened at first (the cluster the page was opened for).
    #[prop_or_default]
    pub focus: bool,
}

/// A row's split button: its module's actions where the operator may run
/// them, then checking it for drift where offered; nothing when neither.
fn row_actions(
    d: &HostDetail,
    name: &str,
    on_action: Option<&Callback<ActionInfo>>,
    on_check_drift: Option<Callback<()>>,
    testid: String,
) -> Option<Html> {
    if on_action.is_none() && on_check_drift.is_none() {
        return None;
    }
    let actions = on_action
        .map(|_| of_module(&d.actions, name))
        .unwrap_or_default();
    let on_run = on_action.cloned().unwrap_or_default();
    Some(html! {
        <ActionsButton {actions} {on_run} {on_check_drift} {testid} />
    })
}

/// One placement of a workload as a row of its page's Placements table:
/// the cluster's Workloads row for it, on the cluster's newest run, opening
/// to the same checks, steps and resources.
#[function_component(PlacementRow)]
pub(super) fn placement_row(props: &PlacementProps) -> Html {
    let d = &props.detail;
    let feed = use_run_report(
        d.latest_run.as_ref().map(|r| RunSource::Host {
            host: d.summary.id.clone(),
            run: r.id.clone(),
        }),
        d.latest_run.clone(),
    );
    let run = feed.report.as_ref();
    let live = run.is_some_and(|r| !run_finished(r.state));
    let now = use_now(if live { 1 } else { 30 });
    let opened = use_state_eq(|| None::<bool>);
    let Some(r) = workload_rows(d, d.latest_run.as_ref())
        .into_iter()
        .find(|r| r.name == props.workload)
    else {
        return html! {};
    };
    let module = run.and_then(|x| x.modules.iter().find(|m| m.name == r.name));
    let is_open = (*opened).unwrap_or_else(|| {
        props.focus
            || r.state == WorkloadState::Failing
            || module.is_some_and(expanded_by_default)
            || d.drift.iter().any(|x| x.module == r.name)
    });
    let toggle = {
        let opened = opened.clone();
        Callback::from(move |e: MouseEvent| {
            let own = e
                .target_dyn_into::<web_sys::Element>()
                .and_then(|t| t.closest("a").ok().flatten())
                .is_some();
            if !own {
                opened.set(Some(!is_open));
            }
        })
    };
    let gone: Vec<&RemovalReport> = run
        .map(|x| {
            x.removals
                .iter()
                .filter(|x| resource_module(&x.resource) == Some(r.name.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let actions = row_actions(
        d,
        &r.name,
        props.on_action.as_ref(),
        props.on_check_drift.clone(),
        format!("actions-{}-{}", d.summary.id, r.name),
    );
    html! { <>
        { workload_row(d, &r, true, true, module, run, is_open, toggle, now, live, actions) }
        if is_open {
            <tr class="yi-table__detail" key={format!("{}-detail", d.summary.id)} data-detail-of={r.name.clone()} data-host={d.summary.id.clone()}>
                <td colspan="6">{ workload_detail(d, &r, true, module, &gone, run, now, live) }</td>
            </tr>
        }
    </> }
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
                    { resource_steps(&resources, &drift, run, d.summary.cluster.as_ref().map(|c| &c.spec), now) }
                }
            </section>
        </div>
    }
}

pub(super) fn state_pill(r: &WorkloadRow, cluster: bool) -> Html {
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
    placement: bool,
    module: Option<&ModuleReport>,
    run: Option<&RunReport>,
    open: bool,
    toggle: Callback<MouseEvent>,
    now: chrono::DateTime<chrono::Utc>,
    live: bool,
    actions: Option<Html>,
) -> Html {
    html! {
        <tr key={r.name.clone()} class={classes!("yi-table__row", "yi-table__row--clickable", open.then_some("yi-table__row--expanded"))}
            id={placement.then(|| format!("on-{}", d.summary.id))} data-host={placement.then(|| d.summary.id.clone())}
            data-workload={cluster.then(|| r.name.clone())} data-module={(!cluster).then(|| r.name.clone())}
            data-state={r.state.label().replace(' ', "-")} onclick={toggle.clone()}>
            <td>
                if placement {
                    <RouteLink to={Route::Cluster { id: d.summary.id.clone() }} classes={classes!("mono", "strong")}>{ d.summary.id.clone() }</RouteLink>
                    { match d.summary.cluster_status.as_ref().and_then(|s| s.steward.clone()) {
                        Some(e) => html! { <span class="cell-sub muted" title="its steward: the agent holding the cluster's lease">{ format!("via {e}") }</span> },
                        None => html! { <span class="cell-sub muted">{ "no steward yet" }</span> },
                    } }
                } else if cluster {
                    <RouteLink to={Route::Workload { name: r.name.clone() }} query={vec![("cluster".to_owned(), d.summary.id.clone())]}
                        classes={classes!("mono", "strong")}>{ r.name.clone() }</RouteLink>
                } else {
                    <span class="mono strong">{ r.name.clone() }</span>
                }
                if !placement && !r.description.is_empty() { <span class="cell-sub">{ r.description.clone() }</span> }
                if !placement && !r.needs.is_empty() { <span class="cell-sub">{ format!("needs {}", r.needs.join(", ")) }</span> }
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
                // Even lines: its state and how long, what it changed and how
                // it read meanwhile, then any deploy progress.
                <div class="run-cell">
                    { match module {
                        Some(m) => {
                            let running = live && m.state == ModuleState::Running;
                            html! { <>
                                <div class="run-cell__line">
                                    <Pill tone={module_tone(m.state)} label={module_label(m.state)} />
                                    if let Some(took) = elapsed_ms(m.duration_ms, m.started_at, m.finished_at, now, running).map(millis) {
                                        <Took label={took} {running} />
                                    }
                                </div>
                                <div class="run-cell__line">
                                    <Changes tally={module_tally(m)} />
                                    { health_bar(m, run.and_then(|x| x.started_at)) }
                                </div>
                            </> }
                        }
                        None => html! { <span class="muted">{ "—" }</span> },
                    } }
                    if let Some(p) = &r.progress {
                        <span class="run-cell__progress mono" data-testid="workload-progress">{ p.clone() }</span>
                    }
                </div>
            </td>
            <td>{ resources_cell(d, &r.name, run) }</td>
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
                <yewi::IconButton icon={yewi::icons::CHEVRON_RIGHT} class="row-toggle" size={yewi::Size::Small}
                    variant={yewi::ButtonVariant::Subtle} on_click={toggle} label={format!("Details of {}", r.name)}
                    attrs={yewi::attrs! { "aria-expanded" => open.to_string(), "title" => if open { "Hide details" } else { "Show health, run and resources" } }} />
            </td>
        </tr>
    }
}

/// A module or workload's resources at a glance: how many, how many removal
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
    let on_change = {
        let (on_pick, newest) = (props.on_pick.clone(), props.newest.clone());
        Callback::from(move |run: String| {
            on_pick.emit((Some(&run) != newest.as_ref()).then_some(run));
        })
    };
    let picker = (!runs.is_empty()).then(|| {
        let listed = shown
            .as_ref()
            .is_some_and(|s| runs.iter().any(|r| &r.id == s));
        let unlisted = shown
            .clone()
            .filter(|_| !listed)
            .map(|s| (AttrValue::from(s), AttrValue::from("newest")));
        let options = unlisted
            .into_iter()
            .chain(runs.iter().enumerate().map(|(i, r)| {
                let state = r
                    .state
                    .map(run_label)
                    .map(str::to_string)
                    .unwrap_or_else(|| r.outcome.replace('_', " "));
                let label = format!(
                    "{}{} · {state} · {}",
                    if i == 0 { "newest · " } else { "" },
                    relative(r.started_at, now),
                    commit(&r.revision)
                );
                (AttrValue::from(r.id.clone()), AttrValue::from(label))
            }))
            .collect::<std::rc::Rc<[_]>>();
        html! {
            <label class="run-picker">
                <span class="muted small">{ "Run" }</span>
                <yewi::NativeSelect label="Run to show" size={yewi::Size::Small} fit=true {options}
                    value={shown.clone().unwrap_or_default()} {on_change} />
            </label>
        }
    });
    html! { <>{ list_error.unwrap_or_default() }{ picker.unwrap_or_default() }</> }
}
