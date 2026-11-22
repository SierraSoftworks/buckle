//! Approving a pending host: the confirmation both the hosts list and the
//! host's own page open. Where the fleet reserves labels the host has no
//! value for yet, the dialog asks for them first, since only the server
//! sets those and a host cannot join without them.

use std::collections::BTreeMap;

use buckle_api::{HostPatch, HostSummary};
use yew::prelude::*;

use super::{Confirm, use_toast};
use crate::api;
use crate::logic::labels::{reserved_values, unset};

/// A page's host approval: what a click on Approve does, and the dialog it
/// opens (render `dialog` on the page).
pub struct HostApproval {
    pub on_ask: Callback<HostSummary>,
    pub dialog: Html,
}

/// Approves the host asked for, setting the reserved labels it lacks
/// first; `on_done` follows (to reload what the page shows).
#[hook]
pub fn use_host_approval(on_done: Callback<()>) -> HostApproval {
    let toast = use_toast();
    let ask = use_state(|| None::<HostSummary>);
    let given = use_state(BTreeMap::<String, String>::new);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let on_ask = {
        let (ask, given, error) = (ask.clone(), given.clone(), error.clone());
        Callback::from(move |h: HostSummary| {
            error.set(None);
            given.set(BTreeMap::new());
            ask.set(Some(h));
        })
    };
    let dialog = (*ask).as_ref().map(|h| {
        let id = h.id.clone();
        let missing = unset(&h.reserved_labels);
        let confirm = {
            let (ask, given, busy, error, toast, on_done, h) = (
                ask.clone(),
                given.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                on_done.clone(),
                h.clone(),
            );
            Callback::from(move |_note: String| {
                let keys: Vec<&str> = unset(&h.reserved_labels)
                    .iter()
                    .map(|r| r.key.as_str())
                    .collect();
                let values = match reserved_values(&keys, &given) {
                    Ok(values) => values,
                    Err(e) => return error.set(Some(e)),
                };
                let (ask, busy, error, toast, on_done, id) = (
                    ask.clone(),
                    busy.clone(),
                    error.clone(),
                    toast.clone(),
                    on_done.clone(),
                    h.id.clone(),
                );
                let mut labels = h.labels.clone();
                let set = !values.is_empty();
                labels.extend(values);
                busy.set(true);
                wasm_bindgen_futures::spawn_local(async move {
                    let mut done = Ok(());
                    if set {
                        let patch = HostPatch {
                            labels: Some(labels),
                            ..Default::default()
                        };
                        done = api::patch_host(&id, &patch).await.map(|_| ());
                    }
                    if done.is_ok() {
                        done = api::host_action(&id, "approve").await.map(|_| ());
                    }
                    busy.set(false);
                    match done {
                        Ok(()) => {
                            toast.ok(format!(
                                "{id} approved: it joins the next rollout's selection."
                            ));
                            ask.set(None);
                            on_done.emit(());
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
        let cancel = {
            let (ask, error) = (ask.clone(), error.clone());
            Callback::from(move |_| {
                ask.set(None);
                error.set(None);
            })
        };
        html! {
            <Confirm title={format!("Approve {id}")} confirm_label={format!("Approve {id}")}
                busy={*busy} error={(*error).clone()} on_confirm={confirm} on_cancel={cancel}>
                <p>{ format!("{id} enrolled and is waiting to join the fleet. Once approved it is assigned the stable revision and is selected by the next rollout like any other host.") }</p>
                <p class="muted">{ "Check the host's labels and facts first if you did not enrol it yourself." }</p>
                { for missing.into_iter().map(|r| {
                    let on_input = {
                        let (given, key) = (given.clone(), r.key.clone());
                        Callback::from(move |value: String| {
                            let mut next = (*given).clone();
                            next.insert(key.clone(), value);
                            given.set(next);
                        })
                    };
                    let hint = match &r.reported {
                        Some(reported) => format!(
                            "The fleet reserves this label: trust decisions read it, so the server sets it. The agent reports '{reported}', which is ignored."
                        ),
                        None => "The fleet reserves this label: trust decisions read it, so the server sets it."
                            .to_owned(),
                    };
                    html! {
                        <yewi::Field label={format!("Label {}", r.key)} required=true {hint}>
                            <yewi::TextInput mono=true value={given.get(&r.key).cloned().unwrap_or_default()} {on_input}
                                attrs={yewi::attrs! { "data-testid" => format!("reserved-{}", r.key) }} />
                        </yewi::Field>
                    }
                }) }
            </Confirm>
        }
    });
    HostApproval {
        on_ask,
        dialog: dialog.unwrap_or_default(),
    }
}
