//! A target's actions behind one split button: the first on its face, the
//! rest in its menu. A click runs the action, or asks first when it wants
//! confirmation, with a note for the audit log.

use buckle_api::ActionInfo;
use yew::prelude::*;

use super::split_button::{MenuItem, SplitButton};
use super::{Confirm, use_toast};
use crate::api;
use crate::logic::actions::{Click, click, label, spans_modules};
use crate::logic::controls::TargetAction;

/// A target's actions as split button items, in declaration order: named
/// for what they do, each with its module as a muted aside when the list
/// spans modules, and in `section` when given.
pub fn action_items(
    actions: &[ActionInfo],
    on_run: &Callback<ActionInfo>,
    section: Option<&str>,
) -> Vec<MenuItem> {
    let qualified = spans_modules(actions);
    actions
        .iter()
        .map(|a| {
            let (on_run, run) = (on_run.clone(), a.clone());
            let mut item = MenuItem::new(
                label(a),
                Callback::from(move |_: MouseEvent| on_run.emit(run.clone())),
            );
            if qualified {
                item = item.aside(a.module.clone());
            }
            if let Some(d) = &a.description {
                item = item.title(d.clone());
            }
            match section {
                Some(s) => item.section(s.to_string()),
                None => item,
            }
        })
        .collect()
}

/// A target's page head as split button items: its own actions (from
/// [`crate::logic::controls::target_actions`], `primary` when the first is
/// what its state calls for), then `extra` (its modules' actions), then
/// Forget, set apart.
pub fn target_items(
    actions: &[TargetAction],
    primary: bool,
    extra: Vec<MenuItem>,
    onclick: impl Fn(TargetAction) -> Callback<MouseEvent>,
) -> Vec<MenuItem> {
    let item = |i: usize, a: TargetAction| {
        let item = MenuItem::new(a.label(), onclick(a));
        match a {
            TargetAction::Forget => item.danger(),
            _ if i == 0 && primary => item.kind(super::Kind::Primary),
            _ => item,
        }
    };
    let (own, forget): (Vec<_>, Vec<_>) = actions
        .iter()
        .copied()
        .enumerate()
        .partition(|(_, a)| *a != TargetAction::Forget);
    own.into_iter()
        .map(|(i, a)| item(i, a))
        .chain(extra)
        .chain(forget.into_iter().map(|(i, a)| item(i, a)))
        .collect()
}

#[derive(Properties, PartialEq)]
pub struct ActionsButtonProps {
    pub actions: Vec<ActionInfo>,
    pub on_run: Callback<ActionInfo>,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// A module's actions as one split button; nothing when there are none.
#[function_component(ActionsButton)]
pub fn actions_button(props: &ActionsButtonProps) -> Html {
    html! {
        <SplitButton items={action_items(&props.actions, &props.on_run, None)} small=true
            menu_label="More actions" testid={props.testid.clone()} />
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
