//! The small pieces every page is built from. All text goes through Yew's
//! escaping interpolation, so a host name, a commit message or a plan diff
//! cannot inject markup into the page an operator is reading.

use yew::prelude::*;

use crate::api::ApiError;
use crate::logic::tone::Tone;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Default,
    /// The one action a view exists for.
    Primary,
    /// An action that destroys or undoes something.
    Danger,
    Subtle,
    /// Rolling the fleet back: orange.
    Caution,
}

#[derive(Properties, PartialEq)]
pub struct ButtonProps {
    pub onclick: Callback<MouseEvent>,
    #[prop_or_default]
    pub kind: Kind,
    #[prop_or_default]
    pub disabled: bool,
    /// Disabled, and says something is happening, so it cannot be sent twice.
    #[prop_or_default]
    pub busy: bool,
    #[prop_or_default]
    pub small: bool,
    #[prop_or_default]
    pub title: Option<AttrValue>,
    #[prop_or_default]
    pub label: Option<AttrValue>,
    #[prop_or_default]
    pub children: Html,
}

#[function_component(Button)]
pub fn button(props: &ButtonProps) -> Html {
    let class = classes!(
        "btn",
        match props.kind {
            Kind::Default => None,
            Kind::Primary => Some("btn--primary"),
            Kind::Danger => Some("btn--danger"),
            Kind::Subtle => Some("btn--subtle"),
            Kind::Caution => Some("btn--caution"),
        },
        props.small.then_some("btn--small"),
        props.busy.then_some("btn--busy"),
    );
    html! {
        <button type="button" {class} onclick={props.onclick.clone()}
            disabled={props.disabled || props.busy} title={props.title.clone()}
            aria-label={props.label.clone()} aria-busy={props.busy.then_some("true")}>
            { props.children.clone() }
        </button>
    }
}

#[derive(Properties, PartialEq)]
pub struct PillProps {
    pub tone: Tone,
    pub label: AttrValue,
    #[prop_or_default]
    pub title: Option<AttrValue>,
}

/// A coloured state label, so a condition looks the same wherever it appears.
#[function_component(Pill)]
pub fn pill(props: &PillProps) -> Html {
    html! {
        <span class={classes!("pill", props.tone.class())} title={props.title.clone()}>
            <span class="pill__dot" aria-hidden="true" />
            { props.label.clone() }
        </span>
    }
}

/// How much a [`Note`] asks of its reader.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum NoteKind {
    #[default]
    Info,
    Warn,
    Danger,
}

#[derive(Properties, PartialEq)]
pub struct NoteProps {
    #[prop_or_default]
    pub kind: NoteKind,
    pub children: Html,
    /// At the end of the line: the hosts or phases it concerns, say.
    #[prop_or_default]
    pub aside: Html,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// A banner, so every note looks the same wherever it comes from: an icon
/// by its kind, its text in the body colour, then its aside.
#[function_component(Note)]
pub fn note(props: &NoteProps) -> Html {
    let (class, icon) = match props.kind {
        NoteKind::Info => ("note--info", super::icons::info()),
        NoteKind::Warn => ("note--warn", super::icons::alert()),
        NoteKind::Danger => ("note--danger", super::icons::alert()),
    };
    html! {
        <div class={classes!("note", class)} data-testid={props.testid.clone()}
            role={(props.kind == NoteKind::Danger).then_some("alert")}>
            <span class="note__icon">{ icon }</span>
            <div class="note__text">{ props.children.clone() }</div>
            { props.aside.clone() }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct HoverTipProps {
    /// What is hovered or focused.
    pub children: Html,
    /// What the popover shows; nothing (and no popover) when empty.
    #[prop_or_default]
    pub tip: Html,
    #[prop_or_default]
    pub label: Option<AttrValue>,
    /// The trigger is a link of its own: no extra tab stop or help cursor,
    /// and the popover follows the link's focus.
    #[prop_or_default]
    pub link: bool,
}

/// A popover under its trigger while it is hovered or focused, positioned
/// against the viewport so that a scrolling table does not clip it.
#[function_component(HoverTip)]
pub fn hover_tip(props: &HoverTipProps) -> Html {
    let at = use_state(|| None::<(f64, f64)>);
    // Measured through a ref: Yew delegates events, so an event's current
    // target is the root, not this trigger.
    let node = use_node_ref();
    let show = {
        let (at, node) = (at.clone(), node.clone());
        move || {
            if let Some(el) = node.cast::<web_sys::Element>() {
                let r = el.get_bounding_client_rect();
                // Kept on screen: the popover is at most `TIP_WIDTH` wide.
                let width = web_sys::window()
                    .and_then(|w| w.inner_width().ok())
                    .and_then(|w| w.as_f64())
                    .unwrap_or(f64::MAX);
                at.set(Some((
                    r.left().min(width - TIP_WIDTH - 8.0).max(8.0),
                    r.bottom(),
                )));
            }
        }
    };
    let (enter, focus) = {
        let (a, b) = (show.clone(), show);
        (
            Callback::from(move |_: MouseEvent| a()),
            Callback::from(move |_: FocusEvent| b()),
        )
    };
    let hide = {
        let at = at.clone();
        Callback::from(move |_: Event| at.set(None))
    };
    if props.tip == Html::default() {
        return props.children.clone();
    }
    let (class, tabindex, label) = if props.link {
        ("tip-link", None, None)
    } else {
        ("tip", Some("0"), props.label.clone())
    };
    html! {
        <span {class} {tabindex} ref={node} aria-label={label}
            onmouseenter={enter} onfocusin={focus}
            onmouseleave={hide.reform(|e: MouseEvent| e.into())} onfocusout={hide.reform(|e: FocusEvent| e.into())}>
            { props.children.clone() }
            if let Some((x, y)) = *at {
                <span class="tip__pop" role="tooltip" style={format!("left: {x:.0}px; top: {:.0}px", y + 4.0)}>
                    { props.tip.clone() }
                </span>
            }
        </span>
    }
}

/// The widest a [`HoverTip`] popover grows (`.tip__pop`'s `max-width`).
const TIP_WIDTH: f64 = 360.0;

#[derive(Properties, PartialEq)]
pub struct PanelProps {
    #[prop_or_default]
    pub title: Option<AttrValue>,
    #[prop_or_default]
    pub subtitle: Option<AttrValue>,
    #[prop_or_default]
    pub actions: Html,
    #[prop_or_default]
    pub id: Option<AttrValue>,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub children: Html,
}

/// A hairline-bordered section with an optional heading row.
#[function_component(Panel)]
pub fn panel(props: &PanelProps) -> Html {
    let heading = props.title.as_ref().map(|title| {
        html! {
            <header class="panel__head">
                <div>
                    <h2 class="panel__title">{ title.clone() }</h2>
                    if let Some(sub) = &props.subtitle {
                        <p class="panel__subtitle">{ sub.clone() }</p>
                    }
                </div>
                <div class="panel__actions">{ props.actions.clone() }</div>
            </header>
        }
    });
    html! {
        <section class={classes!("panel", props.class.clone())} id={props.id.clone()}>
            { heading.unwrap_or_default() }
            <div class="panel__body">{ props.children.clone() }</div>
        </section>
    }
}

#[derive(Properties, PartialEq)]
pub struct PageHeadProps {
    pub title: AttrValue,
    #[prop_or_default]
    pub subtitle: Html,
    #[prop_or_default]
    pub actions: Html,
}

#[function_component(PageHead)]
pub fn page_head(props: &PageHeadProps) -> Html {
    html! {
        <header class="page-head">
            <div class="page-head__text">
                <h1 class="page-head__title">{ props.title.clone() }</h1>
                <div class="page-head__subtitle">{ props.subtitle.clone() }</div>
            </div>
            <div class="page-head__actions">{ props.actions.clone() }</div>
        </header>
    }
}

#[derive(Properties, PartialEq)]
pub struct LoadingProps {
    #[prop_or(AttrValue::from("Loading…"))]
    pub label: AttrValue,
}

#[function_component(Loading)]
pub fn loading(props: &LoadingProps) -> Html {
    html! {
        <div class="state state--loading" role="status" aria-live="polite">
            <span class="spinner" aria-hidden="true" />
            { props.label.clone() }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct EmptyProps {
    pub title: AttrValue,
    #[prop_or_default]
    pub message: Option<AttrValue>,
    #[prop_or_default]
    pub children: Html,
}

/// An empty state says whether nothing being here is a problem, and what to
/// do about it when it is.
#[function_component(Empty)]
pub fn empty(props: &EmptyProps) -> Html {
    html! {
        <div class="state state--empty">
            <p class="state__title">{ props.title.clone() }</p>
            if let Some(message) = &props.message {
                <p class="state__message">{ message.clone() }</p>
            }
            { props.children.clone() }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct ErrorProps {
    pub error: ApiError,
    #[prop_or_default]
    pub retry: Option<Callback<()>>,
}

/// A failed load. A refusal by the access policy is said plainly and offers
/// no retry, since trying again cannot change the answer.
#[function_component(ErrorState)]
pub fn error_state(props: &ErrorProps) -> Html {
    let (title, retry) = match &props.error {
        ApiError::Forbidden(_) => ("Not permitted", None),
        ApiError::NotFound(_) => ("Not found", None),
        ApiError::Unauthorized => ("Signing in again", None),
        ApiError::Network(_) => ("The server could not be reached", props.retry.clone()),
        _ => ("Something went wrong", props.retry.clone()),
    };
    html! {
        <div class="state state--error" role="alert">
            <p class="state__title">{ title }</p>
            <p class="state__message">{ props.error.to_string() }</p>
            if let Some(retry) = retry {
                <Button small=true onclick={Callback::from(move |_| retry.emit(()))}>
                    { super::icons::refresh() }{ "Try again" }
                </Button>
            }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct CopyProps {
    pub text: AttrValue,
    #[prop_or_default]
    pub label: Option<AttrValue>,
}

/// A monospace value with a copy button beside it.
#[function_component(Copyable)]
pub fn copyable(props: &CopyProps) -> Html {
    let text = props.text.clone();
    let onclick = Callback::from(move |_: MouseEvent| crate::util::copy(&text));
    html! {
        <span class="copyable">
            <code class="mono">{ props.label.clone().unwrap_or_else(|| props.text.clone()) }</code>
            <button type="button" class="icon-btn" {onclick}
                aria-label={format!("Copy {}", props.text)} title="Copy">
                { super::icons::copy() }
            </button>
        </span>
    }
}

#[derive(Properties, PartialEq)]
pub struct TraceProps {
    pub id: Option<String>,
    pub url: Option<String>,
}

/// A link to a trace in the tracing UI, or the id to copy when the server
/// does not know one.
#[function_component(TraceLink)]
pub fn trace_link(props: &TraceProps) -> Html {
    match (&props.id, &props.url) {
        (Some(id), Some(url)) => html! {
            <a class="trace-link" href={url.clone()} target="_blank" rel="noopener noreferrer"
                title={format!("Open trace {id}")}>
                { super::icons::trace() }
                <span class="mono">{ crate::logic::format::short(id) }{ "…" }</span>
                { super::icons::external() }
            </a>
        },
        (Some(id), None) => {
            html! { <Copyable text={id.clone()} label={format!("{}…", crate::logic::format::short(id))} /> }
        }
        _ => html! { <span class="muted">{ "—" }</span> },
    }
}

#[derive(Properties, PartialEq)]
pub struct RefreshProps {
    pub loading: bool,
    pub reload: Callback<()>,
}

/// Fetches the page's data again; spins while any fetch is in flight.
#[function_component(Refresh)]
pub fn refresh(props: &RefreshProps) -> Html {
    let reload = props.reload.clone();
    html! {
        <Button small=true kind={Kind::Subtle} busy={props.loading} label="Refresh" title="Refresh"
            onclick={Callback::from(move |_| reload.emit(()))}>
            { super::icons::refresh() }
        </Button>
    }
}

/// `<dl>` rows.
pub fn facts_list(rows: Vec<(&'static str, Html)>) -> Html {
    html! {
        <dl class="kv">
            { for rows.into_iter().map(|(k, v)| html! { <>
                <dt>{ k }</dt>
                <dd>{ v }</dd>
            </> }) }
        </dl>
    }
}
