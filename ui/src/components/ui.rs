//! The small pieces every page is built from, as yewi draws them. Each takes
//! what Buckle has to hand (its own [`Tone`], an [`ApiError`]) and renders
//! the yewi component for it, so a state looks the same wherever it appears.
//! All text goes through Yew's escaping interpolation, so a host name, a
//! commit message or a plan diff cannot inject markup into the page an
//! operator is reading.

use yew::prelude::*;
use yewi::attrs;

use crate::api::ApiError;
use crate::logic::tone::Tone;

impl Tone {
    /// The yewi tone drawn for this one. Rolled back has no tone of its own
    /// there: it is drawn neutral and coloured by its `tone-mute` class.
    pub fn yi(self) -> yewi::Tone {
        match self {
            Tone::Ok => yewi::Tone::Ok,
            Tone::Warn => yewi::Tone::Warn,
            Tone::Danger => yewi::Tone::Danger,
            Tone::Active => yewi::Tone::Active,
            Tone::Info => yewi::Tone::Info,
            Tone::Mute | Tone::Neutral => yewi::Tone::Neutral,
        }
    }
}

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

impl Kind {
    pub fn variant(self) -> yewi::ButtonVariant {
        match self {
            Kind::Default => yewi::ButtonVariant::Default,
            Kind::Primary => yewi::ButtonVariant::Primary,
            Kind::Danger | Kind::Caution => yewi::ButtonVariant::Danger,
            Kind::Subtle => yewi::ButtonVariant::Subtle,
        }
    }

    /// The class that recolours a danger button orange.
    pub fn class(self) -> Option<&'static str> {
        (self == Kind::Caution).then_some("button--caution")
    }
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
    /// The accessible name, where it says more than the button's text.
    #[prop_or_default]
    pub label: Option<AttrValue>,
    #[prop_or_default]
    pub children: Html,
}

#[function_component(Button)]
pub fn button(props: &ButtonProps) -> Html {
    let extra = [("title", &props.title), ("aria-label", &props.label)]
        .into_iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k, v)));
    html! {
        <yewi::Button variant={props.kind.variant()} class={classes!(props.kind.class())}
            size={if props.small { yewi::Size::Small } else { yewi::Size::Medium }}
            disabled={props.disabled} loading={props.busy} on_click={props.onclick.clone()}
            attrs={yewi::Attrs::new(extra)}>
            { props.children.clone() }
        </yewi::Button>
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
        <yewi::Pill tone={props.tone.yi()} class={classes!("pill", props.tone.class())}
            attrs={props.title.clone().map(|t| attrs! { "title" => t }).unwrap_or_default()}>
            { props.label.clone() }
        </yewi::Pill>
    }
}

#[derive(Properties, PartialEq)]
pub struct BadgeProps {
    #[prop_or(Tone::Neutral)]
    pub tone: Tone,
    #[prop_or_default]
    pub mono: bool,
    #[prop_or_default]
    pub title: Option<AttrValue>,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub children: Html,
}

/// A small squared label: a kind, a group or a qualifier, where a [`Pill`]
/// says a state.
#[function_component(Badge)]
pub fn badge(props: &BadgeProps) -> Html {
    let extra = [("title", &props.title), ("data-testid", &props.testid)]
        .into_iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k, v)));
    html! {
        <yewi::Tag tone={props.tone.yi()} mono={props.mono}
            class={classes!("badge", props.tone.class(), props.class.clone())}
            attrs={yewi::Attrs::new(extra)}>
            { props.children.clone() }
        </yewi::Tag>
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
    let tone = match props.kind {
        NoteKind::Info => yewi::Tone::Info,
        NoteKind::Warn => yewi::Tone::Warn,
        NoteKind::Danger => yewi::Tone::Danger,
    };
    html! {
        <yewi::Banner {tone} class="note" actions={props.aside.clone()}
            attrs={props.testid.clone().map(|t| attrs! { "data-testid" => t }).unwrap_or_default()}>
            <span class="note__text">{ props.children.clone() }</span>
        </yewi::Banner>
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

/// A popover under its trigger while it is hovered or focused, in the top
/// layer so that a scrolling table does not clip it.
#[function_component(HoverTip)]
pub fn hover_tip(props: &HoverTipProps) -> Html {
    if props.tip == Html::default() {
        return props.children.clone();
    }
    html! {
        <yewi::Popover trigger={yewi::PopoverTrigger::Hover} placement={yewi::Placement::BottomStart}
            arrow=false focusable={!props.link} label={props.label.clone()}
            class={if props.link { "tip-link" } else { "tip" }} surface_class="tip__pop"
            content={props.tip.clone()}>
            { props.children.clone() }
        </yewi::Popover>
    }
}

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

/// A section of a page: its heading row, then its content flat on the page.
/// The content of a page is not boxed; a [`yewi::Card`] is for the one thing
/// on it that should stand apart.
#[function_component(Panel)]
pub fn panel(props: &PanelProps) -> Html {
    html! {
        <yewi::Panel fill={yewi::PanelFill::Plain} level={2} id={props.id.clone()}
            class={classes!("panel", props.class.clone())}
            title={props.title.clone()} subtitle={props.subtitle.clone()} actions={props.actions.clone()}>
            { props.children.clone() }
        </yewi::Panel>
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
        <yewi::PageHeader class="page-head" title={props.title.clone()}
            subtitle={props.subtitle.clone()} actions={props.actions.clone()} />
    }
}

#[derive(Properties, PartialEq)]
pub struct LoadingProps {
    #[prop_or(AttrValue::from("Loading…"))]
    pub label: AttrValue,
}

#[function_component(Loading)]
pub fn loading(props: &LoadingProps) -> Html {
    html! { <yewi::Loading layout={yewi::LoadingLayout::Block} label={props.label.clone()} /> }
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
        <yewi::EmptyState title={props.title.clone()} actions={props.children.clone()}>
            { props.message.clone() }
        </yewi::EmptyState>
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
        <yewi::ErrorState {title} on_retry={retry}>{ props.error.to_string() }</yewi::ErrorState>
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
    html! {
        <yewi::Copyable value={props.text.clone()} label={format!("Copy {}", props.text)}>
            { props.label.clone().unwrap_or_else(|| props.text.clone()) }
        </yewi::Copyable>
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
        <yewi::IconButton icon={yewi::icons::REFRESH_CW} label="Refresh" variant={yewi::ButtonVariant::Subtle}
            loading={props.loading} on_click={Callback::from(move |_| reload.emit(()))} />
    }
}

/// A term and its value, row by row.
pub fn facts_list(rows: Vec<(&'static str, Html)>) -> Html {
    html! {
        <yewi::DescriptionList class="kv">
            { for rows.into_iter().map(|(k, v)| html! {
                <yewi::DescriptionItem term={html! { k }}>{ v }</yewi::DescriptionItem>
            }) }
        </yewi::DescriptionList>
    }
}
