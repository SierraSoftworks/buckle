//! A collapsible JSON tree for host facts and audit details, built from
//! native `<details>` elements so it is keyboard accessible for free.

use serde_json::Value;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct JsonTreeProps {
    pub value: Value,
    /// Levels open by default.
    #[prop_or(1)]
    pub open: usize,
}

#[function_component(JsonTree)]
pub fn json_tree(props: &JsonTreeProps) -> Html {
    html! { <div class="tree mono">{ node(None, &props.value, 0, props.open) }</div> }
}

fn scalar(v: &Value) -> Html {
    match v {
        Value::Null => html! { <span class="tree__null">{ "null" }</span> },
        Value::Bool(b) => html! { <span class="tree__bool">{ b.to_string() }</span> },
        Value::Number(n) => html! { <span class="tree__num">{ n.to_string() }</span> },
        Value::String(s) => {
            html! { <span class="tree__str">{ super::diff::secret_text(s) }</span> }
        }
        _ => html! {},
    }
}

fn node(key: Option<&str>, v: &Value, depth: usize, open: usize) -> Html {
    let label = key.map(|k| html! { <span class="tree__key">{ k.to_string() }</span> });
    match v {
        Value::Object(map) if !map.is_empty() => html! {
            <details class="tree__node" open={depth < open}>
                <summary>{ label.unwrap_or_else(|| html! { "facts" }) }<span class="tree__count">{ format!(" {{{}}}", map.len()) }</span></summary>
                <div class="tree__children">
                    { for map.iter().map(|(k, v)| node(Some(k), v, depth + 1, open)) }
                </div>
            </details>
        },
        Value::Array(items)
            if !items.is_empty() && items.iter().any(|i| i.is_object() || i.is_array()) =>
        {
            html! {
                <details class="tree__node" open={depth < open}>
                    <summary>{ label.unwrap_or_default() }<span class="tree__count">{ format!(" [{}]", items.len()) }</span></summary>
                    <div class="tree__children">
                        { for items.iter().enumerate().map(|(i, v)| node(Some(&i.to_string()), v, depth + 1, open)) }
                    </div>
                </details>
            }
        }
        Value::Array(items) => html! {
            <div class="tree__leaf">
                { label.unwrap_or_default() }
                <span class="tree__sep">{ ": " }</span>
                <span>{ "[" }</span>
                { for items.iter().enumerate().map(|(i, v)| html! { <>
                    if i > 0 { { ", " } }
                    { scalar(v) }
                </> }) }
                <span>{ "]" }</span>
            </div>
        },
        Value::Object(_) => html! {
            <div class="tree__leaf">{ label.unwrap_or_default() }<span class="tree__sep">{ ": " }</span>{ "{}" }</div>
        },
        scalar_value => html! {
            <div class="tree__leaf">{ label.unwrap_or_default() }<span class="tree__sep">{ ": " }</span>{ scalar(scalar_value) }</div>
        },
    }
}
