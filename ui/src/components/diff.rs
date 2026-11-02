//! Unified diffs and redacted text, with secret references drawn as tokens.

use yew::prelude::*;

use crate::logic::diff::{Segment, lines, rotated_secrets, segments, stats};

/// Redacted text with `«secret:NAME@fp»` tokens shown as tokens.
pub fn secret_text(text: &str) -> Html {
    html! { <>{ for segments(text).into_iter().map(|seg| match seg {
        Segment::Text(t) => html! { { t.to_string() } },
        Segment::Secret { raw, name, fingerprint } => html! {
            <span class="secret" title={format!("Secret {name}, value fingerprint {fingerprint}. The value never leaves the server.")}>
                <span class="visually-hidden">{ raw.to_string() }</span>
                <span aria-hidden="true">
                    { super::icons::key() }
                    <span class="secret__name">{ name.to_string() }</span>
                    <span class="secret__fp">{ format!("@{fingerprint}") }</span>
                </span>
            </span>
        },
    }) }</> }
}

#[derive(Properties, PartialEq)]
pub struct DiffProps {
    pub unified: AttrValue,
}

#[function_component(DiffView)]
pub fn diff_view(props: &DiffProps) -> Html {
    let rotated = rotated_secrets(&props.unified);
    let (added, removed) = stats(&props.unified);
    html! {
        <div class="diff">
            <p class="diff__stats small"><span class="text-ok">{ format!("+{added}") }</span>{ " " }<span class="text-danger">{ format!("−{removed}") }</span></p>
            if !rotated.is_empty() {
                <p class="diff__note">
                    { super::icons::key() }
                    { format!("Rotates {}: the fingerprint of the value changes, the value itself is never shown.", rotated.join(", ")) }
                </p>
            }
            <pre class="diff__body"><code>
                { for lines(&props.unified).into_iter().map(|l| html! {
                    <span class={classes!("diff__line", l.kind.class())}>{ secret_text(l.text) }{ "\n" }</span>
                }) }
            </code></pre>
        </div>
    }
}
