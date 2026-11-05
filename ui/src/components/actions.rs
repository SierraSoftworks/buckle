//! A target's actions behind one split button: the first on its face, the
//! rest in its menu. A click runs the action, or asks first when it wants
//! confirmation, with a note for the audit log.

use buckle_api::ActionInfo;
use yew::prelude::*;

use super::split_button::{MenuItem, SplitButton};
use super::{Confirm, use_toast};
use crate::api;
use crate::logic::actions::{Click, click, label, spans_modules};

#[derive(Properties, PartialEq)]
pub struct ActionsButtonProps {
    pub actions: Vec<ActionInfo>,
    pub on_run: Callback<ActionInfo>,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// Nothing when there are no actions.
#[function_component(ActionsButton)]
pub fn actions_button(props: &ActionsButtonProps) -> Html {
    let Some(first) = props.actions.first() else {
        return html! {};
    };
    let qualified = spans_modules(&props.actions);
    let pick = |a: &ActionInfo| {
        let (on_run, a) = (props.on_run.clone(), a.clone());
        Callback::from(move |e: MouseEvent| {
            // A module head toggles open on a click.
            e.stop_propagation();
            on_run.emit(a.clone())
        })
    };
    let items: Vec<MenuItem> = props.actions[1..]
        .iter()
        .map(|a| MenuItem {
            label: label(a, qualified).into(),
            danger: a.confirm,
            onclick: pick(a),
        })
        .collect();
    html! {
        <span class="actions-button" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
            <SplitButton label={label(first, qualified)} onclick={pick(first)} {items}
                menu_label="More actions" testid={props.testid.clone()} />
        </span>
    }
}

/// A page's action runner: what a click on an [`ActionsButton`] does, and
/// the confirmation dialog it opens (render `dialog` on the page).
pub struct TargetActions {
    pub on_action: Callback<ActionInfo>,
    pub dialog: Html,
}

/// Runs `id`'s actions, asking first for a note where an action wants
/// confirmation; `on_started` follows once one has started (to show its run).
#[hook]
pub fn use_target_actions(id: String, on_started: Callback<()>) -> TargetActions {
    let toast = use_toast();
    let ask = use_state(|| None::<ActionInfo>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    // Runs an action with the note given (empty when it asks no confirmation).
    let start = {
        let (ask, busy, error, id) = (ask.clone(), busy.clone(), error.clone(), id.clone());
        Callback::from(move |(a, note): (ActionInfo, String)| {
            let (ask, busy, error, toast, on_started, id) = (
                ask.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                on_started.clone(),
                id.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let result = api::run_action(&id, &a.module, &a.name, note).await;
                busy.set(false);
                match result {
                    Ok(_) => {
                        toast.ok(format!("{} started on {id}; its run is below.", a.label));
                        ask.set(None);
                        on_started.emit(());
                    }
                    Err(e) if ask.is_some() => error.set(Some(e.to_string())),
                    Err(e) => toast.error(format!("{} did not start: {e}", a.label)),
                }
            });
        })
    };
    let on_action = {
        let (ask, error, start) = (ask.clone(), error.clone(), start.clone());
        Callback::from(move |a: ActionInfo| match click(&a) {
            Click::Confirm => {
                error.set(None);
                ask.set(Some(a));
            }
            Click::Run => start.emit((a, String::new())),
        })
    };
    let dialog = (*ask).as_ref().map(|a| {
        let confirm = {
            let (start, a) = (start.clone(), a.clone());
            Callback::from(move |note: String| start.emit((a.clone(), note)))
        };
        let cancel = {
            let ask = ask.clone();
            Callback::from(move |_| ask.set(None))
        };
        html! {
            <Confirm title={format!("{} on {id}", a.label)} confirm_label={a.label.clone()} danger=true
                note_required=true note_placeholder="Why, for the audit log" busy={*busy}
                error={(*error).clone()} on_confirm={confirm} on_cancel={cancel}>
                <p>
                    <span class="badge mono">{ format!("{}/{}", a.module, a.name) }</span>{ " " }
                    <span class="badge">{ a.kind.clone() }</span>
                </p>
                if let Some(d) = &a.description { <p>{ d.clone() }</p> }
                <p class="muted">{ "It runs now, from the applied revision; its run appears below." }</p>
            </Confirm>
        }
    });
    TargetActions {
        on_action,
        dialog: dialog.unwrap_or_default(),
    }
}
