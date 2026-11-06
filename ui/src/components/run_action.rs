//! Action rollouts, through the stable revision's phases (approvals, waves,
//! baseline and bake, failure budgets), opening the rollout once started:
//! "Run action…" on the Hosts page runs a module's action on the hosts a
//! selector matches; a workload's actions run across its placements from
//! the Workloads views ([`use_workload_rollout`]).

use buckle_api::{ActionInfo, FleetAction, StartActionRollout};
use web_sys::{HtmlInputElement, HtmlSelectElement};
use yew::prelude::*;
use yew_router::prelude::*;

use super::split_button::{MenuItem, SplitButton};
use super::{Confirm, use_toast};
use crate::api;
use crate::app::Route;
use crate::hooks::{use_debounced, use_fetch};
use crate::logic::actions::{fleet_key, rollout_item_label, rollout_label};

#[derive(Properties, PartialEq)]
pub struct RunActionProps {
    pub on_close: Callback<()>,
}

#[function_component(RunAction)]
pub fn run_action(props: &RunActionProps) -> Html {
    let navigator = use_navigator();
    let toast = use_toast();
    let selector = use_state(String::new);
    let settled = use_debounced((*selector).clone(), 300);
    let offered = use_fetch(
        settled.clone(),
        |s| async move { api::fleet_actions(&s).await },
    );
    let picked = use_state(|| None::<String>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    // Workloads' actions run from the Workloads views, across placements.
    let actions: Vec<FleetAction> = offered
        .data
        .as_ref()
        .map(|d| d.1.iter().filter(|f| !f.workload).cloned().collect())
        .unwrap_or_default();
    let current = picked
        .as_ref()
        .and_then(|k| actions.iter().find(|f| &fleet_key(f) == k))
        .or(actions.first())
        .cloned();

    let on_selector = {
        let selector = selector.clone();
        Callback::from(move |e: InputEvent| {
            selector.set(e.target_unchecked_into::<HtmlInputElement>().value())
        })
    };
    let on_pick = {
        let picked = picked.clone();
        Callback::from(move |e: Event| {
            picked.set(Some(e.target_unchecked_into::<HtmlSelectElement>().value()))
        })
    };
    let on_confirm = {
        let (busy, error, toast, current, selector) = (
            busy.clone(),
            error.clone(),
            toast.clone(),
            current.clone(),
            (*selector).clone(),
        );
        Callback::from(move |note: String| {
            let Some(f) = current.clone() else {
                error.set(Some(
                    "No host the selector matches offers an action.".into(),
                ));
                return;
            };
            let (busy, error, toast, navigator) = (
                busy.clone(),
                error.clone(),
                toast.clone(),
                navigator.clone(),
            );
            let selector = selector.trim().to_owned();
            start(
                StartActionRollout {
                    module: f.action.module.clone(),
                    action: f.action.name.clone(),
                    workload: false,
                    selector: (!selector.is_empty()).then_some(selector),
                    note,
                },
                f.action.label.clone(),
                (busy, error, toast, navigator),
            );
        })
    };
    let on_cancel = {
        let on_close = props.on_close.clone();
        Callback::from(move |_| on_close.emit(()))
    };
    let confirm = current.as_ref().is_some_and(|f| f.action.confirm);
    let label = current
        .as_ref()
        .map(rollout_label)
        .unwrap_or_else(|| "Run".into());
    let listed_error = offered.error.as_ref().map(|e| e.to_string());

    html! {
        <Confirm title="Run a module action across hosts" confirm_label={label} danger={confirm}
            note=true note_required={confirm} note_placeholder="Why, for the audit log"
            busy={*busy} error={(*error).clone().or(listed_error)} {on_confirm} {on_cancel}>
            <label class="field">
                <span class="field__label">{ "Targets" }</span>
                <input class="input mono" type="text" aria-label="Selector" spellcheck="false"
                    placeholder={"every host offering the action, or a selector: host.labels.role == \"web\""}
                    value={(*selector).clone()} oninput={on_selector} />
            </label>
            <label class="field">
                <span class="field__label">{ "Action" }</span>
                <select class="input" aria-label="Action" onchange={on_pick} disabled={actions.is_empty()}>
                    { for actions.iter().map(|f| {
                        let key = fleet_key(f);
                        let selected = current.as_ref().is_some_and(|c| fleet_key(c) == key);
                        html! { <option value={key} {selected}>{ format!("{} · {}", f.action.module, f.action.label) }</option> }
                    }) }
                </select>
            </label>
            if actions.is_empty() && !offered.loading {
                <p class="muted">{ "No host the selector matches offers a module action." }</p>
            }
            if let Some(f) = &current {
                if let Some(d) = &f.action.description { <p>{ d.clone() }</p> }
                <p class="muted small" data-testid="run-action-targets">{ format!("Runs on {}", f.targets.join(", ")) }</p>
            }
            <p class="muted">{ "Each host runs it from the revision it runs, through the stable revision's phases: approvals, waves, baseline and bake, and failure budgets. Workloads' actions are on the Workloads pages." }</p>
        </Confirm>
    }
}

type Starting = (
    UseStateHandle<bool>,
    UseStateHandle<Option<String>>,
    super::toast::Toaster,
    Option<Navigator>,
);

/// Starts an action rollout, then opens it.
fn start(body: StartActionRollout, label: String, (busy, error, toast, navigator): Starting) {
    busy.set(true);
    wasm_bindgen_futures::spawn_local(async move {
        let result = api::start_action_rollout(body).await;
        busy.set(false);
        match result {
            Ok(r) => {
                toast.ok(format!("{label} is rolling out."));
                if let Some(n) = &navigator {
                    n.push(&Route::Rollout { id: r.id });
                }
            }
            Err(e) => error.set(Some(e.to_string())),
        }
    });
}

#[derive(Properties, PartialEq)]
pub struct WorkloadRolloutProps {
    pub workload: String,
    pub action: ActionInfo,
    pub on_close: Callback<()>,
}

/// A workload's action across every cluster running it, placement by
/// placement through the stable revision's phases.
#[function_component(WorkloadRollout)]
pub fn workload_rollout(props: &WorkloadRolloutProps) -> Html {
    let navigator = use_navigator();
    let toast = use_toast();
    let offered = use_fetch((), |_| async { api::fleet_actions("").await });
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);
    let (workload, action) = (props.workload.clone(), props.action.clone());
    let found = offered.data.as_ref().and_then(|d| {
        d.1.iter()
            .find(|f| f.workload && f.action.module == workload && f.action.name == action.name)
            .cloned()
    });

    let on_confirm = {
        let (busy, error, toast, found) =
            (busy.clone(), error.clone(), toast.clone(), found.clone());
        let (workload, action) = (workload.clone(), action.clone());
        Callback::from(move |note: String| {
            if found.is_none() {
                error.set(Some(format!(
                    "No cluster running {workload} offers {} now.",
                    action.label
                )));
                return;
            }
            start(
                StartActionRollout {
                    module: workload.clone(),
                    action: action.name.clone(),
                    workload: true,
                    selector: None,
                    note,
                },
                format!("{} of {workload}", action.label),
                (
                    busy.clone(),
                    error.clone(),
                    toast.clone(),
                    navigator.clone(),
                ),
            );
        })
    };
    let on_cancel = {
        let on_close = props.on_close.clone();
        Callback::from(move |_| on_close.emit(()))
    };
    let label = found
        .as_ref()
        .map(rollout_label)
        .unwrap_or_else(|| action.label.clone());
    let listed_error = offered.error.as_ref().map(|e| e.to_string());

    html! {
        <Confirm title={format!("{} {workload} on every cluster", action.label)} confirm_label={label}
            danger={action.confirm} note=true note_required={action.confirm} note_placeholder="Why, for the audit log"
            busy={*busy} error={(*error).clone().or(listed_error)} {on_confirm} {on_cancel}>
            <p>
                <span class="badge mono">{ format!("{workload}/{}", action.name) }</span>{ " " }
                <span class="badge">{ action.kind.clone() }</span>
            </p>
            if let Some(d) = &action.description { <p>{ d.clone() }</p> }
            { match &found {
                Some(f) => html! { <p class="muted small" data-testid="workload-rollout-targets">{ format!("Runs on {}", f.targets.join(", ")) }</p> },
                None if !offered.loading => html! { <p class="muted">{ "No cluster running it offers this action now: its clusters must be active, with an applied revision." }</p> },
                None => html! {},
            } }
            <p class="muted">{ "Each placement runs it from the revision its cluster runs, in turn through the stable revision's phases: approvals, waves, baseline and bake, and failure budgets." }</p>
        </Confirm>
    }
}

/// A page's workload-wide actions: a click names the workload and action,
/// and opens the rollout dialog (render `dialog` on the page).
pub struct WorkloadRollouts {
    pub on_action: Callback<(String, ActionInfo)>,
    pub dialog: Html,
}

#[hook]
pub fn use_workload_rollout() -> WorkloadRollouts {
    let open = use_state(|| None::<(String, ActionInfo)>);
    let on_action = {
        let open = open.clone();
        Callback::from(move |picked| open.set(Some(picked)))
    };
    let dialog = match &*open {
        Some((workload, action)) => {
            let on_close = {
                let open = open.clone();
                Callback::from(move |_| open.set(None))
            };
            html! { <WorkloadRollout workload={workload.clone()} action={action.clone()} {on_close} /> }
        }
        None => html! {},
    };
    WorkloadRollouts { on_action, dialog }
}

#[derive(Properties, PartialEq)]
pub struct WorkloadActionsProps {
    pub workload: String,
    /// What its placements offer ([`crate::logic::workloads::FleetWorkload::actions`]).
    pub actions: Vec<ActionInfo>,
    pub on_action: Callback<(String, ActionInfo)>,
    #[prop_or_default]
    pub small: bool,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// A workload's actions across all its placements as one split button, the
/// first on its face; nothing when it offers none.
#[function_component(WorkloadActions)]
pub fn workload_actions(props: &WorkloadActionsProps) -> Html {
    let items = props
        .actions
        .iter()
        .map(|a| {
            let (on_action, picked) =
                (props.on_action.clone(), (props.workload.clone(), a.clone()));
            MenuItem::new(
                rollout_item_label(a),
                Callback::from(move |_: MouseEvent| on_action.emit(picked.clone())),
            )
            .title(format!(
                "Roll it out across every cluster running {}{}",
                props.workload,
                a.description
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            ))
        })
        .collect::<Vec<_>>();
    html! {
        <SplitButton {items} small={props.small} menu_label={format!("More {} actions", props.workload)}
            testid={props.testid.clone()} />
    }
}
