//! A target's actions behind one split button: the first on its face, the
//! rest in its menu. A click runs the action, or asks first when it wants
//! confirmation, with a note for the audit log.

use crate::components::ui::Badge;
use buckle_api::{ActionInfo, ActionInstances};
use yew::prelude::*;

use super::split_button::{MenuItem, SplitButton};
use super::{Confirm, Segmented, use_toast};
use crate::api;
use crate::logic::actions::{
    Click, INSTANCE_CHOICES, click, label, parse_instances, spans_modules,
};

#[derive(Properties, PartialEq)]
pub struct InstancesPickerProps {
    pub value: ActionInstances,
    pub on_change: Callback<ActionInstances>,
}

/// Where an action offering the choice runs: one allocation or every one.
#[function_component(InstancesPicker)]
pub fn instances_picker(props: &InstancesPickerProps) -> Html {
    let options = INSTANCE_CHOICES
        .iter()
        .map(|(v, l)| (AttrValue::from(v.as_str()), AttrValue::from(*l)))
        .collect::<Vec<_>>();
    html! {
            <yewi::Field group=true label="Runs in">
                <Segmented label="Where it runs" testid="action-instances" {options}
                    value={AttrValue::from(props.value.as_str())}
                    on_change={props.on_change.reform(|v: AttrValue| parse_instances(&v))} />
    </yewi::Field>
        }
}
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
    /// Set where the row may check its module (a placement) for drift: a
    /// "Check drift" item after its actions.
    #[prop_or_default]
    pub on_check_drift: Option<Callback<()>>,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// A module's actions as one split button, then checking it for drift
/// where offered; nothing when there is neither.
#[function_component(ActionsButton)]
pub fn actions_button(props: &ActionsButtonProps) -> Html {
    let mut items = action_items(&props.actions, &props.on_run, None);
    if let Some(check) = &props.on_check_drift {
        items.push(drift_item(
            check,
            "Re-plan it against what is running now and report what differs",
        ));
    }
    html! {
        <SplitButton {items} small=true menu_label="More actions" testid={props.testid.clone()} />
    }
}

/// The "Check drift" item a split button offers, running `check` on click.
pub fn drift_item(check: &Callback<()>, title: &str) -> MenuItem {
    MenuItem::new(
        crate::logic::controls::TargetAction::CheckDrift.label(),
        check.reform(|_: MouseEvent| ()),
    )
    .title(title.to_owned())
}

/// What a page's drift checks do: ask each named target (a cluster, for the
/// named workload's placement) to check for drift now, toast the outcome,
/// then `on_asked` (to reload what the check reports).
#[hook]
pub fn use_drift_check(on_asked: Callback<()>) -> Callback<(Vec<String>, Option<String>)> {
    let toast = use_toast();
    Callback::from(move |(targets, workload): (Vec<String>, Option<String>)| {
        let (toast, on_asked) = (toast.clone(), on_asked.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let mut asked = Vec::new();
            for id in &targets {
                match api::check_drift(id, workload.clone()).await {
                    Ok(_) => asked.push(id.clone()),
                    Err(e) => toast.error(format!("{id} could not check for drift: {e}")),
                }
            }
            if asked.is_empty() {
                return;
            }
            toast.ok(match (&workload, asked.as_slice()) {
                (Some(w), [one]) => format!("Asked {one} to check {w} for drift."),
                (Some(w), many) => format!("Asked {} clusters to check {w} for drift.", many.len()),
                (None, [one]) => format!("Asked {one} to check for drift."),
                (None, many) => format!("Asked {} targets to check for drift.", many.len()),
            });
            on_asked.emit(());
        });
    })
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
    let any = use_actions(on_started);
    TargetActions {
        on_action: any.on_action.reform(move |a| (id.clone(), a)),
        dialog: any.dialog,
    }
}

/// Several targets' actions from one page: a click names the target.
pub struct AnyTargetActions {
    pub on_action: Callback<(String, ActionInfo)>,
    pub dialog: Html,
}

/// [`use_target_actions`] for a page acting on more than one target (a
/// workload's placements, each on its cluster).
#[hook]
pub fn use_actions(on_started: Callback<()>) -> AnyTargetActions {
    let toast = use_toast();
    let ask = use_state(|| None::<(String, ActionInfo)>);
    let chosen = use_state(|| None::<ActionInstances>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    // Runs an action with the note given (empty when it asks no
    // confirmation), where the dialog says when it offers the choice.
    let start = {
        let (ask, busy, error, chosen) = (ask.clone(), busy.clone(), error.clone(), chosen.clone());
        Callback::from(move |(id, a, note): (String, ActionInfo, String)| {
            let instances = a.instances.map(|d| (*chosen).unwrap_or(d));
            let (ask, busy, error, toast, on_started) = (
                ask.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                on_started.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let result = api::run_action(&id, &a.module, &a.name, note, instances).await;
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
        let (ask, error, start, chosen) =
            (ask.clone(), error.clone(), start.clone(), chosen.clone());
        Callback::from(move |(id, a): (String, ActionInfo)| match click(&a) {
            Click::Confirm | Click::Choose => {
                error.set(None);
                chosen.set(a.instances);
                ask.set(Some((id, a)));
            }
            Click::Run => start.emit((id, a, String::new())),
        })
    };
    let dialog = (*ask).as_ref().map(|(id, a)| {
        let confirm = {
            let (start, id, a) = (start.clone(), id.clone(), a.clone());
            Callback::from(move |note: String| start.emit((id.clone(), a.clone(), note)))
        };
        let cancel = {
            let ask = ask.clone();
            Callback::from(move |_| ask.set(None))
        };
        let picker = a.instances.map(|d| {
            let chosen = chosen.clone();
            html! {
                <InstancesPicker value={(*chosen).unwrap_or(d)}
                    on_change={Callback::from(move |v| chosen.set(Some(v)))} />
            }
        });
        html! {
            <Confirm title={format!("{} on {id}", a.label)} confirm_label={a.label.clone()} danger={a.confirm}
                note=true note_required={a.confirm} note_placeholder="Why, for the audit log" busy={*busy}
                error={(*error).clone()} on_confirm={confirm} on_cancel={cancel}>
                <p>
                    <Badge mono=true>{ format!("{}/{}", a.module, a.name) }</Badge>{ " " }
                    <Badge>{ a.kind.clone() }</Badge>
                </p>
                if let Some(d) = &a.description { <p>{ d.clone() }</p> }
                { picker.unwrap_or_default() }
                <p class="muted">{ "It runs now, from the applied revision; its run appears below." }</p>
            </Confirm>
        }
    });
    AnyTargetActions {
        on_action,
        dialog: dialog.unwrap_or_default(),
    }
}
