//! Audit (design §12): every operator action and controller transition with
//! actor, time and trace, filtered with a filt-rs expression the way
//! automate's activity page is. The expression is checked as it is typed,
//! with the parser the server uses, and lives in the URL so a filtered view
//! can be linked to.

use buckle_api::{AuditEntry, LiveEvent};
use serde::{Deserialize, Serialize};
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::Route;
use crate::components::{
    Button, Empty, ErrorState, JsonTree, Loading, PageHead, Panel, Pill, Refresh, TraceLink,
};
use crate::hooks::{use_debounced, use_fetch, use_now};
use crate::logic::filter::{AUDIT_FIELDS, Check, and, check, equals};
use crate::logic::format::{relative, timestamp};
use crate::logic::tone::outcome_tone;
use crate::store::use_live;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    filter: String,
}

const EXAMPLES: &[(&str, &str)] = &[
    (
        "Operators only",
        r#"actor != "controller" && actor != "system""#,
    ),
    ("Rollout decisions", r#"action startswith "rollout.""#),
    (
        "Refusals and failures",
        r#"outcome in ["denied", "failure"]"#,
    ),
    ("Last hour", "at > now() - 1h"),
];

#[function_component(Audit)]
pub fn audit() -> Html {
    let now = use_now(30);
    let live = use_live();
    let location = use_location();
    let navigator = use_navigator();
    let applied = location
        .and_then(|l| l.query::<Query>().ok())
        .unwrap_or_default()
        .filter;
    let draft = use_state(|| applied.clone());
    let settled = use_debounced((*draft).clone(), 250);
    let more = use_state(Vec::<AuditEntry>::new);
    let next = use_state(|| None::<u64>);
    let loading_more = use_state(|| false);
    let expanded = use_state(|| None::<u64>);

    // The URL follows the box once the expression is worth sending.
    {
        let navigator = navigator.clone();
        let applied = applied.clone();
        use_effect_with(settled.clone(), move |expr| {
            let ok = matches!(check(expr), Check::Valid | Check::Unchecked | Check::Empty);
            if ok
                && *expr != applied
                && let Some(n) = navigator
            {
                let _ = n.replace_with_query(
                    &Route::Audit,
                    &Query {
                        filter: expr.clone(),
                    },
                );
            }
            || ()
        });
    }
    // And the box follows the URL (a link, or the back button).
    {
        let draft = draft.clone();
        use_effect_with(applied.clone(), move |a| {
            if *draft != *a {
                draft.set(a.clone());
            }
            || ()
        });
    }

    let page = use_fetch(
        applied.clone(),
        |f| async move { api::audit(&f, None).await },
    );
    let seen_seq = page.data.as_ref().map(|d| d.0).unwrap_or(0);
    let fresh = live
        .log
        .since(seen_seq)
        .map(|events| {
            events
                .iter()
                .filter(|e| matches!(***e, LiveEvent::Audit { .. }))
                .count()
        })
        .unwrap_or(0);
    {
        let (more, next) = (more.clone(), next.clone());
        let first_next = page
            .value()
            .and_then(|p| p.next.as_ref().and_then(|n| n.parse().ok()));
        use_effect_with((applied.clone(), first_next), move |(_, n)| {
            more.set(Vec::new());
            next.set(*n);
            || ()
        });
    }

    let set_filter = {
        let draft = draft.clone();
        Callback::from(move |value: String| draft.set(value))
    };
    let oninput = {
        let set_filter = set_filter.clone();
        Callback::from(move |e: InputEvent| {
            set_filter.emit(e.target_unchecked_into::<HtmlInputElement>().value())
        })
    };
    let narrow = |field: &'static str, value: String| {
        let (draft, set_filter) = (draft.clone(), set_filter.clone());
        Callback::from(move |_: MouseEvent| set_filter.emit(and(&draft, &equals(field, &value))))
    };
    let load_more = {
        let (more, next, loading_more, applied) = (
            more.clone(),
            next.clone(),
            loading_more.clone(),
            applied.clone(),
        );
        Callback::from(move |_: MouseEvent| {
            let Some(before) = *next else { return };
            let (more, next, loading_more, applied) = (
                more.clone(),
                next.clone(),
                loading_more.clone(),
                applied.clone(),
            );
            loading_more.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(p) = api::audit(&applied, Some(before)).await {
                    let mut all = (*more).clone();
                    all.extend(p.items);
                    more.set(all);
                    next.set(p.next.and_then(|n| n.parse().ok()));
                }
                loading_more.set(false);
            });
        })
    };

    let status = match check(&draft) {
        Check::Empty => {
            html! { <span class="filter__hint muted">{ format!("Fields: {}", AUDIT_FIELDS.join(", ")) }</span> }
        }
        Check::Valid => html! { <span class="filter__hint text-ok">{ "Valid filter" }</span> },
        Check::Unchecked => {
            html! { <span class="filter__hint muted">{ "Uses `matches`; the server will check the pattern." }</span> }
        }
        Check::Invalid { message, advice } => html! {
            <span class="filter__hint text-danger" role="alert" data-testid="filter-error">
                { message }{ if advice.is_empty() { String::new() } else { format!(" {}", advice.join(" ")) } }
            </span>
        },
    };
    let invalid = matches!(check(&draft), Check::Invalid { .. });

    let rows = page.value().map(|p| {
        let mut items = p.items.clone();
        items.extend(more.iter().cloned());
        (items, p.total)
    });
    let body = match (&rows, &page.error) {
        (Some((items, _)), _) if items.is_empty() => html! {
            <Empty title={if applied.is_empty() { "Nothing audited yet" } else { "No entries match" }}
                message={(!applied.is_empty()).then(|| AttrValue::from("Loosen the filter, or clear it to see everything."))} />
        },
        (Some((items, total)), _) => html! { <>
            if fresh > 0 {
                <div class="banner banner--info" role="status">
                    { format!("{fresh} new {} since this page loaded. ", if fresh == 1 { "entry" } else { "entries" }) }
                    <Button small=true onclick={{ let r = page.reload.clone(); Callback::from(move |_| r.emit(())) }}>{ "Show" }</Button>
                </div>
            }
            <p class="muted small">{ format!("{} of {total} entries", items.len()) }</p>
            <div class="table-wrap">
                <table class="table table--audit" data-testid="audit-table">
                    <thead><tr>
                        <th scope="col">{ "When" }</th><th scope="col">{ "Actor" }</th><th scope="col">{ "Action" }</th>
                        <th scope="col">{ "Outcome" }</th><th scope="col">{ "Subject" }</th><th scope="col">{ "Message" }</th>
                        <th scope="col">{ "Trace" }</th>
                    </tr></thead>
                    <tbody>
                        { for items.iter().map(|e| {
                            let open = *expanded == Some(e.id);
                            let toggle = { let (expanded, id) = (expanded.clone(), e.id); Callback::from(move |_: MouseEvent| expanded.set(if open { None } else { Some(id) })) };
                            html! { <>
                                <tr key={e.id}>
                                    <td title={timestamp(e.at)}>{ relative(e.at, now) }</td>
                                    <td><button type="button" class="link-btn" title="Filter by this actor" onclick={narrow("actor", e.actor.clone())}>{ e.actor.clone() }</button></td>
                                    <td><button type="button" class="link-btn mono" title="Filter by this action" onclick={narrow("action", e.action.clone())}>{ e.action.clone() }</button></td>
                                    <td><Pill tone={outcome_tone(&e.outcome)} label={e.outcome.clone()} /></td>
                                    <td>{ match &e.subject {
                                        Some(s) => html! { <button type="button" class="link-btn mono" title="Filter by this subject" onclick={narrow("subject", s.clone())}>{ s.clone() }</button> },
                                        None => html! { <span class="muted">{ "—" }</span> },
                                    } }</td>
                                    <td>
                                        { e.message.clone() }
                                        if e.detail.is_some() {
                                            <button type="button" class="link-btn link-btn--aside small" aria-expanded={open.to_string()} onclick={toggle}>
                                                { if open { "hide detail" } else { "detail" } }
                                            </button>
                                        }
                                    </td>
                                    <td><TraceLink id={e.trace_id.clone()} url={None::<String>} /></td>
                                </tr>
                                if open {
                                    if let Some(detail) = &e.detail {
                                        <tr class="table__detail" key={format!("{}-d", e.id)}><td colspan="7"><JsonTree value={detail.clone()} open={3} /></td></tr>
                                    }
                                }
                            </> }
                        }) }
                    </tbody>
                </table>
            </div>
            if next.is_some() {
                <div class="more"><Button onclick={load_more} busy={*loading_more}>{ "Load older entries" }</Button></div>
            }
        </> },
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={page.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading the audit log…" /> },
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Audit" subtitle={html! { "Every operator action and controller transition." }}
                actions={html! { <Refresh loading={page.loading} reload={page.reload.clone()} /> }} />
            <Panel>
                <div class="filter">
                    <label class="field">
                        <span class="field__label">{ "Filter (filt-rs)" }</span>
                        <input class={classes!("input", "mono", invalid.then_some("input--invalid"))} type="search"
                            value={(*draft).clone()} {oninput} spellcheck="false" autocomplete="off"
                            placeholder={r#"actor == "alice" && action startswith "rollout.""#}
                            aria-invalid={invalid.to_string()} data-testid="audit-filter" />
                    </label>
                    { status }
                    <div class="chips" role="group" aria-label="Examples">
                        { for EXAMPLES.iter().map(|(label, expr)| {
                            let set_filter = set_filter.clone();
                            html! { <button type="button" class="chip" title={expr.to_string()} onclick={Callback::from(move |_| set_filter.emit(expr.to_string()))}>{ *label }</button> }
                        }) }
                        if !draft.is_empty() {
                            <button type="button" class="chip" onclick={{ let s = set_filter.clone(); Callback::from(move |_| s.emit(String::new())) }}>{ "Clear" }</button>
                        }
                    </div>
                </div>
                { body }
            </Panel>
        </div>
    }
}
