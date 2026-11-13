//! Dialogs: yewi's modal `Dialog`, and the confirmation every irreversible
//! action goes through (design §12: approval, forget, abort and rollback).
//!
//! The confirmation names what is about to happen to what, can require a
//! checkbox ("I have reviewed the 4 destructive steps") or the name of the
//! thing typed out, and labels its button with the action rather than "Yes",
//! so the two buttons cannot be told apart only by position. Buckle adds the
//! note that is recorded in the audit log with the action.
//!
//! Both are mounted only while they are open, so each opening starts from
//! nothing typed and nothing ticked.

use yew::prelude::*;
use yewi::{ConfirmDialog, Dialog, Field, Size, TextArea};

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
    html! {
        <Dialog open=true title={props.title.clone()} on_close={props.on_close.clone()}
            size={if props.wide { Size::Large } else { Size::Medium }} footer={props.footer.clone()}>
            { props.children.clone() }
        </Dialog>
    }
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
    /// The note is a required reason: the action stays disabled until one
    /// is written (implies `note`).
    #[prop_or_default]
    pub note_required: bool,
    #[prop_or_default]
    pub error: Option<String>,
    pub on_confirm: Callback<String>,
    pub on_cancel: Callback<()>,
}

#[function_component(Confirm)]
pub fn confirm(props: &ConfirmProps) -> Html {
    let note = use_state(String::new);
    let on_note = {
        let note = note.clone();
        Callback::from(move |text: String| note.set(text))
    };
    let on_confirm = {
        let (on_confirm, note) = (props.on_confirm.clone(), note.clone());
        Callback::from(move |()| on_confirm.emit((*note).trim().to_string()))
    };
    // A click on the word to type copies it (yewi shows it, but not as
    // something to copy). It still has to be pasted: copying confirms nothing.
    let on_copy = Callback::from(|e: MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(word) = e
            .target_dyn_into::<web_sys::Element>()
            .and_then(|t| t.closest(".yi-confirm__word").ok().flatten())
        else {
            return;
        };
        crate::util::copy(&word.text_content().unwrap_or_default());
        let input = word
            .closest(".yi-field")
            .ok()
            .flatten()
            .and_then(|f| f.query_selector("input").ok().flatten())
            .and_then(|i| i.dyn_into::<web_sys::HtmlElement>().ok());
        if let Some(input) = input {
            let _ = input.focus();
        }
    });
    html! {
        <div class="confirm-host" onclick={on_copy}>
        <ConfirmDialog open=true class="confirm" title={props.title.clone()}
            confirm_label={props.confirm_label.clone()} danger={props.danger} busy={props.busy}
            disabled={props.note_required && note.trim().is_empty()}
            acknowledge={props.acknowledge.clone()} type_to_confirm={props.type_to_confirm.clone()}
            error={props.error.clone().map(AttrValue::from)}
            {on_confirm} on_cancel={props.on_cancel.clone()}>
            { props.children.clone() }
            if props.note || props.note_required {
                <Field label={html! { { if props.note_required { "Reason for the audit log (required)" } else { "Note for the audit log (optional)" } } }}
                    required={props.note_required}>
                    <TextArea rows={2} value={(*note).clone()} on_input={on_note}
                        placeholder={props.note_placeholder.clone()} />
                </Field>
            }
        </ConfirmDialog>
        </div>
    }
}
