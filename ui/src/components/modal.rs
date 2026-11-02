//! Dialogs: a modal that holds the keyboard while it is open, and the
//! confirmation every irreversible action goes through (design §12:
//! approval, forget, abort and rollback).
//!
//! The confirmation names what is about to happen to what, can require a
//! checkbox ("I have reviewed the 4 destructive steps") or the name of the
//! thing typed out, and labels its button with the action rather than "Yes",
//! so the two buttons cannot be told apart only by position.

use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use super::ui::{Button, Kind};

const FOCUSABLE: &str = "button:not([disabled]), [href], input:not([disabled]), select, textarea, [tabindex]:not([tabindex='-1'])";

fn focusables(root: &web_sys::Element) -> Vec<HtmlElement> {
    let Ok(list) = root.query_selector_all(FOCUSABLE) else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|n| n.dyn_into::<HtmlElement>().ok())
        .collect()
}

#[derive(Properties, PartialEq)]
pub struct ModalProps {
    pub title: AttrValue,
    pub on_close: Callback<()>,
    #[prop_or_default]
    pub wide: bool,
    #[prop_or_default]
    pub children: Html,
    #[prop_or_default]
    pub footer: Html,
}

#[function_component(Modal)]
pub fn modal(props: &ModalProps) -> Html {
    let dialog = use_node_ref();
    let title_id = use_memo((), |_| {
        format!("dialog-{}", (crate::util::random() * 1e9) as u64)
    });

    // Focus moves into the dialog when it opens and back to where it came
    // from when it closes.
    {
        let dialog = dialog.clone();
        use_effect_with((), move |_| {
            let previous = gloo_utils_active();
            if let Some(el) = dialog.cast::<web_sys::Element>() {
                let first = focusables(&el)
                    .into_iter()
                    .find(|e| e.get_attribute("data-autofocus").is_some())
                    .or_else(|| focusables(&el).into_iter().next());
                match first {
                    Some(f) => {
                        let _ = f.focus();
                    }
                    None => {
                        if let Ok(h) = el.dyn_into::<HtmlElement>() {
                            let _ = h.focus();
                        }
                    }
                }
            }
            move || {
                if let Some(p) = previous {
                    let _ = p.focus();
                }
            }
        });
    }

    let onkeydown = {
        let (dialog, on_close) = (dialog.clone(), props.on_close.clone());
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                e.prevent_default();
                on_close.emit(());
                return;
            }
            if e.key() != "Tab" {
                return;
            }
            let Some(root) = dialog.cast::<web_sys::Element>() else {
                return;
            };
            let items = focusables(&root);
            let (Some(first), Some(last)) = (items.first(), items.last()) else {
                return;
            };
            let active = gloo_utils_active();
            let at = |el: &HtmlElement| active.as_ref().is_some_and(|a| a == el);
            if e.shift_key() && at(first) {
                e.prevent_default();
                let _ = last.focus();
            } else if !e.shift_key() && at(last) {
                e.prevent_default();
                let _ = first.focus();
            }
        })
    };

    let on_backdrop = {
        let on_close = props.on_close.clone();
        Callback::from(move |e: MouseEvent| {
            if e.target() == e.current_target() {
                on_close.emit(());
            }
        })
    };
    let close = {
        let on_close = props.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };

    html! {
        <div class="modal" onclick={on_backdrop}>
            <div ref={dialog} class={classes!("modal__dialog", props.wide.then_some("modal__dialog--wide"))}
                role="dialog" aria-modal="true" aria-labelledby={(*title_id).clone()} tabindex="-1"
                {onkeydown}>
                <header class="modal__head">
                    <h2 class="modal__title" id={(*title_id).clone()}>{ props.title.clone() }</h2>
                    <button type="button" class="icon-btn" aria-label="Close" onclick={close}>
                        { super::icons::close() }
                    </button>
                </header>
                <div class="modal__body">{ props.children.clone() }</div>
                <footer class="modal__foot">{ props.footer.clone() }</footer>
            </div>
        </div>
    }
}

fn gloo_utils_active() -> Option<HtmlElement> {
    web_sys::window()?
        .document()?
        .active_element()?
        .dyn_into::<HtmlElement>()
        .ok()
}

#[derive(Properties, PartialEq)]
pub struct ConfirmProps {
    pub title: AttrValue,
    /// What will happen, to what.
    #[prop_or_default]
    pub children: Html,
    /// The action, as the button says it: "Approve 12 hosts", "Forget pi-1".
    pub confirm_label: AttrValue,
    #[prop_or_default]
    pub danger: bool,
    #[prop_or_default]
    pub busy: bool,
    /// A statement the operator must tick before the action is enabled.
    #[prop_or_default]
    pub acknowledge: Option<AttrValue>,
    /// A word the operator must type before the action is enabled.
    #[prop_or_default]
    pub type_to_confirm: Option<AttrValue>,
    /// Offer a note, which is recorded in the audit log with the action.
    #[prop_or_default]
    pub note: bool,
    #[prop_or_default]
    pub note_placeholder: Option<AttrValue>,
    #[prop_or_default]
    pub error: Option<String>,
    pub on_confirm: Callback<String>,
    pub on_cancel: Callback<()>,
}

#[function_component(Confirm)]
pub fn confirm(props: &ConfirmProps) -> Html {
    let acked = use_state(|| false);
    let typed = use_state(String::new);
    let note = use_state(String::new);

    let ready = (props.acknowledge.is_none() || *acked)
        && props
            .type_to_confirm
            .as_ref()
            .is_none_or(|w| typed.trim() == w.as_str());

    let on_ack = {
        let acked = acked.clone();
        Callback::from(move |e: Event| {
            acked.set(e.target_unchecked_into::<HtmlInputElement>().checked());
        })
    };
    let on_type = {
        let typed = typed.clone();
        Callback::from(move |e: InputEvent| {
            typed.set(e.target_unchecked_into::<HtmlInputElement>().value());
        })
    };
    let on_note = {
        let note = note.clone();
        Callback::from(move |e: InputEvent| {
            note.set(e.target_unchecked_into::<HtmlTextAreaElement>().value());
        })
    };
    let confirm = {
        let (on_confirm, note) = (props.on_confirm.clone(), note.clone());
        Callback::from(move |_: MouseEvent| {
            if ready {
                on_confirm.emit((*note).trim().to_string());
            }
        })
    };
    let cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_: MouseEvent| on_cancel.emit(()))
    };

    let footer = html! { <>
        <Button onclick={cancel}>{ "Cancel" }</Button>
        <Button kind={if props.danger { Kind::Danger } else { Kind::Primary }}
            disabled={!ready} busy={props.busy} onclick={confirm}>
            { props.confirm_label.clone() }
        </Button>
    </> };

    html! {
        <Modal title={props.title.clone()} on_close={props.on_cancel.clone()} {footer}>
            <div class="confirm">
                { props.children.clone() }
                if let Some(statement) = &props.acknowledge {
                    <label class="confirm__ack">
                        <input type="checkbox" checked={*acked} onchange={on_ack} data-autofocus="" />
                        <span>{ statement.clone() }</span>
                    </label>
                }
                if let Some(word) = &props.type_to_confirm {
                    <label class="field">
                        <span class="field__label">{ "Type " }<code class="mono">{ word.clone() }</code>{ " to confirm" }</span>
                        <input class="input mono" type="text" value={(*typed).clone()} oninput={on_type}
                            autocomplete="off" spellcheck="false" data-autofocus="" />
                    </label>
                }
                if props.note {
                    <label class="field">
                        <span class="field__label">{ "Note for the audit log (optional)" }</span>
                        <textarea class="input" rows="2" value={(*note).clone()} oninput={on_note}
                            placeholder={props.note_placeholder.clone().unwrap_or_default()} />
                    </label>
                }
                if let Some(error) = &props.error {
                    <p class="confirm__error" role="alert">{ error.clone() }</p>
                }
            </div>
        </Modal>
    }
}
