//! Audit (design §12): every operator action and controller transition with
//! actor, time and trace, filtered with a filt-rs expression the way
//! automate's activity page is. The expression is checked as it is typed,
//! with the parser the server uses, and lives in the URL so a filtered view
//! can be linked to.

use buckle_api::{AuditEntry, LiveEvent, PROGRESS_ACTIONS};
use serde::{Deserialize, Serialize};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::Route;
use crate::components::{
    Button, Empty, ErrorState, JsonTree, Loading, PageHead, Pill, Refresh, RouteLink, TraceLink,
};
use crate::hooks::{use_fetch, use_now};
use crate::logic::filter::{and, equals};
use crate::logic::format::{relative, subject_rollout_name, timestamp};
use crate::logic::tone::outcome_tone;
use crate::store::use_live;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Query {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    filter: String,
    /// Show the controller's progress transitions (hidden by default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    progress: bool,
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

/// The `word.word.word` name of the rollout an entry is about.
fn rollout_subject(e: &AuditEntry) -> Option<String> {
    e.subject.as_deref().and_then(subject_rollout_name)
}

/// Where a host's, rollout's or revision's entry links to, by its action's kind.
fn subject_route(e: &AuditEntry) -> Option<Route> {
    let subject = e.subject.clone()?;
    match e.action.split('.').next()? {
        "host" | "agent" => Some(Route::Host { id: subject }),
        "target" => Some(Route::Cluster { id: subject }),
        "rollout" | "phase" => Some(Route::rollout(&subject)),
        "revision" => Some(Route::Rollouts),
        _ => None,
    }
}

/// An entry's message, with its rollout's id written as the rollout's name.
fn message(e: &AuditEntry) -> String {
    match (&e.subject, rollout_subject(e)) {
        (Some(id), Some(name)) => e.message.replace(id.as_str(), &name),
        _ => e.message.clone(),
    }
}

#[function_component(Audit)]
pub fn audit() -> Html {
    let now = use_now(30);
    let live = use_live();
    let location = use_location();
    let navigator = use_navigator();
    let query = location
        .and_then(|l| l.query::<Query>().ok())
        .unwrap_or_default();
    let (applied, progress) = (query.filter.clone(), query.progress);
    let more = use_state(Vec::<AuditEntry>::new);
    let next = use_state(|| None::<u64>);
    let loading_more = use_state(|| false);
    let expanded = use_state(|| None::<u64>);

    let page = use_fetch((applied.clone(), progress), |(f, progress)| async move {
        api::audit(&f, None, progress).await
    });
    let seen_seq = page.data.as_ref().map(|d| d.0).unwrap_or(0);
    let fresh = live
        .log
        .since(seen_seq)
        .map(|events| {
            events
                .iter()
                .filter(|e| match &***e {
                    LiveEvent::Audit { entry } => {
                        progress || !PROGRESS_ACTIONS.contains(&entry.action.as_str())
                    }
                    _ => false,
                })
                .count()
        })
        .unwrap_or(0);
    {
        let (more, next) = (more.clone(), next.clone());
        let first_next = page
            .value()
            .and_then(|p| p.next.as_ref().and_then(|n| n.parse().ok()));
        use_effect_with((applied.clone(), progress, first_next), move |(_, _, n)| {
            more.set(Vec::new());
            next.set(*n);
            || ()
        });
    }

    // A filter the page applies goes to the URL, where the app bar's filter
    // box and the list both read it.
    let set_filter = {
        let navigator = navigator.clone();
        Callback::from(move |filter: String| {
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(&Route::Audit, &Query { filter, progress });
            }
        })
    };
    let toggle_progress = {
        let (navigator, applied) = (navigator.clone(), applied.clone());
        Callback::from(move |progress: bool| {
            if let Some(n) = &navigator {
                let _ = n.replace_with_query(
                    &Route::Audit,
                    &Query {
                        filter: applied.clone(),
                        progress,
                    },
                );
            }
        })
    };
    let narrow = |field: &'static str, value: String| {
        let (applied, set_filter) = (applied.clone(), set_filter.clone());
        Callback::from(move |_: MouseEvent| set_filter.emit(and(&applied, &equals(field, &value))))
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
                if let Ok(p) = api::audit(&applied, Some(before), progress).await {
                    let mut all = (*more).clone();
                    all.extend(p.items);
                    more.set(all);
                    next.set(p.next.and_then(|n| n.parse().ok()));
                }
                loading_more.set(false);
            });
        })
    };

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
                <yewi::Banner actions={html! { <Button small=true onclick={{ let r = page.reload.clone(); Callback::from(move |_| r.emit(())) }}>{ "Show" }</Button> }}>
                    { format!("{fresh} new {} since this page loaded.", if fresh == 1 { "entry" } else { "entries" }) }
                </yewi::Banner>
            }
            <p class="muted small">{ format!("{} of {total} entries", items.len()) }</p>
            <yewi::SimpleTable variant={yewi::TableVariant::Ruled} wrap=true class="table--audit" attrs={yewi::attrs! { "data-testid" => "audit-table" }}>
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
                                    <td>{ match (&e.subject, rollout_subject(e)) {
                                        (Some(s), Some(name)) => html! { <span class="audit__rollout">
                                            <RouteLink to={Route::rollout(s)} classes="mono" title={format!("Rollout {s}")}>{ name }</RouteLink>
                                            <button type="button" class="link-btn link-btn--aside small" title={format!("Filter by this rollout ({s})")}
                                                aria-label={format!("Filter by rollout {s}")} onclick={narrow("subject", s.clone())}>{ "filter" }</button>
                                        </span> },
                                        // A host or revision opens where it lives; the
                                        // filter stays beside it.
                                        (Some(s), None) => match subject_route(e) {
                                            Some(to) => html! { <span class="audit__rollout">
                                                <RouteLink {to} classes="mono">{ s.clone() }</RouteLink>
                                                <button type="button" class="link-btn link-btn--aside small" title="Filter by this subject"
                                                    aria-label={format!("Filter by {s}")} onclick={narrow("subject", s.clone())}>{ "filter" }</button>
                                            </span> },
                                            None => html! { <button type="button" class="link-btn mono" title="Filter by this subject" onclick={narrow("subject", s.clone())}>{ s.clone() }</button> },
                                        },
                                        (None, _) => html! { <span class="muted">{ "—" }</span> },
                                    } }</td>
                                    <td>
                                        { message(e) }
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
                                        <tr class="yi-table__detail" key={format!("{}-d", e.id)}><td colspan="7"><JsonTree value={detail.clone()} open={3} /></td></tr>
                                    }
                                }
                            </> }
                        }) }
                    </tbody>
            </yewi::SimpleTable>
            if next.is_some() {
                <div class="more"><Button onclick={load_more} busy={*loading_more}>{ "Load older entries" }</Button></div>
            }
        </> },
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={page.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading the audit log…" /> },
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Audit"
                actions={html! { <Refresh loading={page.loading} reload={page.reload.clone()} /> }} />
            <div class="toolbar">
                <div class="chips" role="group" aria-label="Example filters">
                    <span class="muted small">{ "Examples" }</span>
                    { for EXAMPLES.iter().map(|(label, expr)| {
                        let set_filter = set_filter.clone();
                        html! {
                            <yewi::Button size={yewi::Size::Small} pressed={Some(applied == *expr)} attrs={yewi::attrs! { "title" => *expr }}
                                on_click={Callback::from(move |_| set_filter.emit(expr.to_string()))}>{ *label }</yewi::Button>
                        }
                    }) }
                    if !applied.is_empty() {
                        <yewi::Button size={yewi::Size::Small} variant={yewi::ButtonVariant::Subtle}
                            on_click={{ let s = set_filter.clone(); Callback::from(move |_| s.emit(String::new())) }}>{ "Clear" }</yewi::Button>
                    }
                </div>
                <yewi::ToggleSwitch checked={progress} on_change={toggle_progress} size={yewi::Size::Small}
                    attrs={yewi::attrs! { "data-testid" => "audit-progress", "title" => "Controller transitions that only show a host moving through a rollout's ordinary steps: planned, baseline, applying, baking." }}>
                    { "Show rollout progress" }
                </yewi::ToggleSwitch>
            </div>
            { body }
        </div>
    }
}
