//! Suspending one module of a host, or one workload of a cluster, and
//! resuming it: the dialog a row's Suspend… or Resume… opens (a reason,
//! always, and when suspending a day to look at it again), and how a
//! suspended unit is marked.

use buckle_api::PausedUnit;
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::split_button::MenuItem;
use super::ui::Badge;
use super::{Confirm, Kind, Pill, use_toast};
use crate::api;
use crate::hooks::Fetch;
use crate::logic::format::timestamp;
use crate::logic::pause::{
    Offer, PauseAsk, attribution, changes_units, review_at, review_due, review_text,
};
use crate::logic::tone::Tone;
use crate::store::use_live;

/// A row's Suspend… or Resume… item, set apart from its actions.
pub fn pause_item(offer: Offer, workload: bool, onclick: Callback<()>) -> MenuItem {
    let item = MenuItem::new(offer.label(), onclick.reform(|_: MouseEvent| ()))
        .title(offer.title(workload))
        .section("");
    match (offer, workload) {
        // Suspending a workload stops its jobs.
        (Offer::Suspend, true) => item.kind(Kind::Caution),
        _ => item,
    }
}

/// How a suspended unit is marked: who suspended it, when and why on hover,
/// and louder once its review is due.
pub fn suspended_pill(p: &PausedUnit, now: DateTime<Utc>) -> Html {
    let mut title = format!(
        "Suspended {} ({}): {}",
        attribution(p, now),
        timestamp(p.at),
        p.note
    );
    if let Some(review) = review_text(p, now) {
        title.push_str(&format!(" · {review}"));
    }
    let tone = if review_due(p, now) {
        Tone::Warn
    } else {
        Tone::Mute
    };
    html! { <Pill {tone} label="suspended" {title} /> }
}

/// Fetches `fetch` again once a unit was suspended or resumed anywhere
/// after it was fetched (by another operator, or the API), so what a page
/// shows of suspended units stays current.
#[hook]
pub fn use_unit_changes<T: 'static>(fetch: &Fetch<T>) {
    let live = use_live();
    let changed = fetch.data.as_ref().is_some_and(|d| {
        live.log
            .since(d.0)
            .is_some_and(|events| events.iter().any(|e| changes_units(e)))
    });
    let reload = fetch.reload.clone();
    use_effect_with(changed, move |changed| {
        if *changed {
            reload.emit(());
        }
        || ()
    });
}

/// A page's Suspend and Resume: what a click on a row's item does, and the
/// dialog it opens (render `dialog` on the page).
pub struct UnitPause {
    pub on_ask: Callback<PauseAsk>,
    pub dialog: Html,
}

/// Suspends or resumes the unit asked for, with the reason given; `on_done`
/// follows (to reload what the page shows).
#[hook]
pub fn use_unit_pause(on_done: Callback<()>) -> UnitPause {
    let toast = use_toast();
    let ask = use_state(|| None::<PauseAsk>);
    let review = use_state(String::new);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let on_ask = {
        let (ask, review, error) = (ask.clone(), review.clone(), error.clone());
        Callback::from(move |a: PauseAsk| {
            error.set(None);
            review.set(String::new());
            ask.set(Some(a));
        })
    };
    let dialog = (*ask).as_ref().map(|a| {
        let resume = a.paused.is_some();
        let confirm = {
            let (ask, review, busy, error, toast, on_done, a) = (
                ask.clone(),
                review.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                on_done.clone(),
                a.clone(),
            );
            Callback::from(move |note: String| {
                let at = match review_at(&review) {
                    Ok(at) => at.filter(|_| !resume),
                    Err(e) => return error.set(Some(e)),
                };
                let (ask, busy, error, toast, on_done, a) = (
                    ask.clone(),
                    busy.clone(),
                    error.clone(),
                    toast.clone(),
                    on_done.clone(),
                    a.clone(),
                );
                busy.set(true);
                wasm_bindgen_futures::spawn_local(async move {
                    let done =
                        api::pause_unit(&a.target, &a.unit, a.workload, resume, note, at).await;
                    busy.set(false);
                    match done {
                        Ok(run) => {
                            toast.ok(match (resume, run.is_some()) {
                                (false, true) => format!(
                                    "{} is suspended on {}; its @suspend hook is running.",
                                    a.unit, a.target
                                ),
                                (false, false) => {
                                    format!("{} is suspended on {}.", a.unit, a.target)
                                }
                                (true, true) => format!(
                                    "{}'s @resume hook is running on {}; the pause lifts once it has succeeded.",
                                    a.unit, a.target
                                ),
                                (true, false) => format!("{} is resumed on {}.", a.unit, a.target),
                            });
                            ask.set(None);
                            on_done.emit(());
                        }
                        Err(e) => error.set(Some(e.to_string())),
                    }
                });
            })
        };
        let cancel = {
            let ask = ask.clone();
            Callback::from(move |_| ask.set(None))
        };
        let on_review = {
            let review = review.clone();
            Callback::from(move |v: String| review.set(v))
        };
        let (verb, what) = match (resume, a.workload) {
            (false, false) => (
                "Suspend",
                "Rollouts, drift correction and actions leave the module alone on this host until it is resumed, and its checks read paused and gate nothing. Its '@suspend' hook runs first, when the module declares one.",
            ),
            (false, true) => (
                "Suspend",
                "Its jobs on this cluster are stopped. Rollouts, drift correction and actions then leave the workload alone here until it is resumed.",
            ),
            (true, false) => (
                "Resume",
                "Its '@resume' hook runs first, when the module declares one, and the pause lifts once it has succeeded. The host's next drift pass then brings the module to the applied revision.",
            ),
            (true, true) => (
                "Resume",
                "The pause lifts, and the workload is applied here again in the stragglers stage of the stable revision's rollout.",
            ),
        };
        html! {
            <Confirm title={format!("{verb} {} on {}", a.unit, a.target)} confirm_label={verb}
                danger={!resume && a.workload} note_required=true
                note_placeholder={if resume { "Why it can run again" } else { "Why, shown with it while it is suspended" }}
                busy={*busy} error={(*error).clone()} on_confirm={confirm} on_cancel={cancel}>
                <p>
                    <Badge mono=true>{ a.unit.clone() }</Badge>{ " " }
                    <Badge>{ if a.workload { "workload" } else { "module" } }</Badge>
                </p>
                if let Some(p) = &a.paused {
                    <p data-testid="pause-note">
                        { format!("Suspended {}: ", attribution(p, Utc::now())) }
                        <span class="strong">{ p.note.clone() }</span>
                    </p>
                }
                <p class="muted">{ what }</p>
                if !resume {
                    <yewi::Field label="Review on (optional)"
                        hint="From that day (UTC) it is on the attention list. Nothing else happens.">
                        <yewi::TextInput kind={yewi::TextInputKind::Date} value={(*review).clone()} on_input={on_review}
                            attrs={yewi::attrs! { "data-testid" => "pause-review" }} />
                    </yewi::Field>
                }
            </Confirm>
        }
    });
    UnitPause {
        on_ask,
        dialog: dialog.unwrap_or_default(),
    }
}
