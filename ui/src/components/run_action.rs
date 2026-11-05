//! "Run action…": an action rollout over the targets a selector matches,
//! through the stable revision's phases (approvals, waves, baseline and
//! bake, failure budgets). Opens the rollout once started.

use buckle_api::{FleetAction, StartActionRollout};
use web_sys::{HtmlInputElement, HtmlSelectElement};
use yew::prelude::*;
use yew_router::prelude::*;

use super::{Confirm, use_toast};
use crate::api;
use crate::app::Route;
use crate::hooks::{use_debounced, use_fetch};
use crate::logic::actions::{fleet_key, rollout_label};

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

    let actions: Vec<FleetAction> = offered
        .data
        .as_ref()
        .map(|d| d.1.clone())
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
                    "No target offers an action the selector matches.".into(),
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
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let result = api::start_action_rollout(StartActionRollout {
                    module: f.action.module.clone(),
                    action: f.action.name.clone(),
                    selector: (!selector.is_empty()).then_some(selector),
                    note,
                })
                .await;
                busy.set(false);
                match result {
                    Ok(r) => {
                        toast.ok(format!("{} is rolling out.", f.action.label));
                        if let Some(n) = &navigator {
                            n.push(&Route::Rollout { id: r.id });
                        }
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
            });
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
        <Confirm title="Run an action across the fleet" confirm_label={label} danger={confirm}
            note=true note_required={confirm} note_placeholder="Why, for the audit log"
            busy={*busy} error={(*error).clone().or(listed_error)} {on_confirm} {on_cancel}>
            <label class="field">
                <span class="field__label">{ "Targets" }</span>
                <input class="input mono" type="text" aria-label="Selector" spellcheck="false"
                    placeholder={"every target offering the action, or a selector: host.labels.role == \"web\""}
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
                <p class="muted">{ "No target the selector matches offers an action." }</p>
            }
            if let Some(f) = &current {
                if let Some(d) = &f.action.description { <p>{ d.clone() }</p> }
                <p class="muted small" data-testid="run-action-targets">{ format!("Runs on {}", f.targets.join(", ")) }</p>
            }
            <p class="muted">{ "Each target runs it from the revision it runs, through the stable revision's phases: approvals, waves, baseline and bake, and failure budgets." }</p>
        </Confirm>
    }
}
