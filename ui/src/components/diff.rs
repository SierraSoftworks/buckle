//! Unified diffs and redacted text, with secret references and fact
//! placeholders drawn as tokens.

use std::rc::Rc;

use buckle_api::FleetPlan;
use yew::prelude::*;

use crate::logic::diff::{Segment, lines, rotated_secrets, segments, stats};

/// Exemplars for the `«fact:PATH»` tokens below it, provided by a fleet-wide
/// plan view: a [`FleetPlan`] holding only its `facts`.
#[derive(Clone, PartialEq, Default)]
pub struct FactExemplars(pub Rc<FleetPlan>);

#[derive(Properties, PartialEq)]
struct FactProps {
    raw: AttrValue,
    path: AttrValue,
}

/// A fact placeholder; hovering or focusing it shows sample values.
#[function_component(FactToken)]
fn fact_token(props: &FactProps) -> Html {
    let exemplars = use_context::<FactExemplars>();
    let at = use_state(|| None::<(f64, f64)>);
    let samples = exemplars.and_then(|e| e.0.fact(&props.path).cloned());
    // Measured through a ref: Yew delegates events, so an event's current
    // target is the root, not this token.
    let node = use_node_ref();
    let show = {
        let (at, node) = (at.clone(), node.clone());
        move || {
            if let Some(el) = node.cast::<web_sys::Element>() {
                let r = el.get_bounding_client_rect();
                at.set(Some((r.left(), r.bottom())));
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
    // `tailscale.ip` for `host.facts.tailscale.ip`; `host.id` as it is.
    let label = props
        .path
        .strip_prefix("host.facts.")
        .unwrap_or(&props.path)
        .to_string();
    html! {
        <span class="fact" tabindex="0" data-fact={props.path.clone()} ref={node}
            onmouseenter={enter} onfocus={focus}
            onmouseleave={hide.reform(|e: MouseEvent| e.into())} onblur={hide.reform(|e: FocusEvent| e.into())}>
            <span class="visually-hidden">{ props.raw.clone() }</span>
            <span aria-hidden="true">{ super::icons::host() }<span class="fact__path">{ label }</span></span>
            if let (Some((x, y)), Some(s)) = (*at, samples.as_ref()) {
                <span class="fact__pop" role="tooltip" style={format!("left: {x:.0}px; top: {:.0}px", y + 4.0)}>
                    <span class="fact__pop-head mono">{ props.path.clone() }</span>
                    <span class="muted small">{ format!("{}, for example:", crate::logic::format::count(s.distinct as usize, "distinct value", "distinct values")) }</span>
                    <span class="fact__samples">
                        { for s.samples.iter().map(|x| html! { <>
                            <span class="mono fact__host">{ x.host.clone() }</span>
                            <span class="mono">{ x.value.clone() }</span>
                        </> }) }
                    </span>
                </span>
            }
        </span>
    }
}

/// Redacted text with `«secret:NAME@fp»` and `«fact:PATH»` tokens shown as tokens.
pub fn secret_text(text: &str) -> Html {
    html! { <>{ for segments(text).into_iter().map(|seg| match seg {
        Segment::Text(t) => html! { { t.to_string() } },
        Segment::Fact { raw, path } => html! { <FactToken raw={raw.to_string()} path={path.to_string()} /> },
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
