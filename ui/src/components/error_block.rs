//! A structured error ([`ErrorView`]) laid out the way a reader takes it in:
//! what went wrong as the headline, the chain of causes beneath it
//! (outermost first), then what to try, with a tag saying who should act on
//! it. An error that only arrived as a string is shown whole in a monospace
//! block, never run into a sentence.

use buckle_api::ErrorView;
use yew::prelude::*;

use super::diff::secret_text;
use super::icons;
use crate::logic::tone::{Tone, error_kind_class, error_kind_label, error_kind_title};

#[derive(Properties, PartialEq)]
pub struct ErrorBlockProps {
    /// The structured error, when the producer sent one.
    #[prop_or_default]
    pub error: Option<ErrorView>,
    /// The flattened string, shown only when there is no structured error.
    #[prop_or_default]
    pub raw: Option<AttrValue>,
    /// How loudly to say it: a skip reason is a note, not a failure.
    #[prop_or(Tone::Danger)]
    pub tone: Tone,
    /// What it is about, above the headline (a step or module).
    #[prop_or_default]
    pub context: Option<AttrValue>,
    /// Tighter, for a table cell.
    #[prop_or_default]
    pub compact: bool,
}

#[function_component(ErrorBlock)]
pub fn error_block(props: &ErrorBlockProps) -> Html {
    let context = props
        .context
        .as_ref()
        .map(|c| html! { <p class="error-block__context mono">{ c.clone() }</p> });
    let base = classes!(
        "error-block",
        props.tone.class(),
        props.compact.then_some("error-block--compact")
    );
    match (&props.error, &props.raw) {
        (Some(e), _) => html! {
            <div class={classes!(base, error_kind_class(e.kind))} data-testid="error-block">
                { context.unwrap_or_default() }
                <div class="error-block__head">
                    <span class="error-block__icon">{ icons::alert() }</span>
                    <p class="error-block__message">{ secret_text(&e.message) }</p>
                    <span class="error-block__kind" title={error_kind_title(e.kind)}>{ error_kind_label(e.kind) }</span>
                </div>
                if !e.causes.is_empty() {
                    <div class="error-block__section">
                        <p class="error-block__label">{ "Caused by" }</p>
                        <ol class="error-block__causes">
                            { for e.causes.iter().map(|c| html! { <li>{ secret_text(c) }</li> }) }
                        </ol>
                    </div>
                }
                if !e.advice.is_empty() {
                    <div class="error-block__section">
                        <p class="error-block__label">{ "To fix" }</p>
                        <ul class="error-block__advice">
                            { for e.advice.iter().map(|a| html! { <li>{ secret_text(a) }</li> }) }
                        </ul>
                    </div>
                }
            </div>
        },
        (None, Some(raw)) if !raw.trim().is_empty() => html! {
            <div class={classes!(base, "error-block--raw")} data-testid="error-block">
                { context.unwrap_or_default() }
                <pre class="error-block__raw">{ secret_text(raw) }</pre>
            </div>
        },
        _ => html! {},
    }
}
