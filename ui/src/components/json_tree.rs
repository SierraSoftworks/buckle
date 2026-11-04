//! A collapsible JSON tree for host facts and audit details, built from
//! native `<details>` elements so it is keyboard accessible for free.
//!
//! A collapsed node renders only its summary: its children are created when
//! it is first opened. A host's facts run to thousands of nodes, and adding
//! them all to the page at once is what browser extensions that watch the
//! DOM (password managers walking every added node for form fields) choke
//! on, freezing the page for seconds.

use std::rc::Rc;

use serde_json::Value;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct JsonTreeProps {
    pub value: Value,
    /// Levels open by default.
    #[prop_or(1)]
    pub open: usize,
}

/// The tree's value, shared by every node; nodes compare it by identity so
/// that re-rendering one does not compare the whole document.
#[derive(Clone)]
struct Root(Rc<Value>);

impl PartialEq for Root {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[function_component(JsonTree)]
pub fn json_tree(props: &JsonTreeProps) -> Html {
    let root = use_memo(props.value.clone(), |v| Root(Rc::new(v.clone())));
    html! {
        <div class="tree mono">
            { node(&root, None, "", &root.0, 0, props.open) }
        </div>
    }
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

/// `key` escaped as one JSON pointer segment (RFC 6901).
fn segment(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Whether `v` is drawn as a collapsible branch rather than one line.
fn branch(v: &Value) -> bool {
    match v {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => items.iter().any(|i| i.is_object() || i.is_array()),
        _ => false,
    }
}

/// The node at `pointer` (`v`, already resolved): a [`Branch`] for objects
/// and arrays of containers, a leaf line otherwise.
fn node(
    root: &Root,
    key: Option<&str>,
    pointer: &str,
    v: &Value,
    depth: usize,
    open: usize,
) -> Html {
    if branch(v) {
        return html! {
            <Branch root={root.clone()} label={key.map(AttrValue::from)} pointer={AttrValue::from(pointer.to_owned())}
                {depth} {open} />
        };
    }
    let label = key.map(|k| html! { <span class="tree__key">{ k.to_string() }</span> });
    match v {
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

#[derive(Properties, PartialEq)]
struct BranchProps {
    root: Root,
    /// The key or index under its parent; `None` for the root.
    label: Option<AttrValue>,
    /// Where the branch is in `root`, as a JSON pointer.
    pointer: AttrValue,
    depth: usize,
    open: usize,
}

/// An object or array of containers, its children rendered only while open.
#[function_component(Branch)]
fn branch_node(props: &BranchProps) -> Html {
    let expanded = use_state_eq(|| props.depth < props.open);
    let Some(v) = props.root.0.pointer(&props.pointer) else {
        return html! {};
    };
    // The summary toggles the state, and the state the element: a toggle
    // the browser made by itself would show an open node with no children.
    let onclick = {
        let expanded = expanded.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            expanded.set(!*expanded);
        })
    };
    let (root, pointer, depth, open) = (&props.root, &props.pointer, props.depth, props.open);
    let (count, children): (String, Option<Html>) = match v {
        Value::Object(map) => (
            format!(" {{{}}}", map.len()),
            (*expanded).then(|| {
                map.iter()
                    .map(|(k, v)| {
                        node(
                            root,
                            Some(k),
                            &format!("{pointer}/{}", segment(k)),
                            v,
                            depth + 1,
                            open,
                        )
                    })
                    .collect()
            }),
        ),
        Value::Array(items) => (
            format!(" [{}]", items.len()),
            (*expanded).then(|| {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        node(
                            root,
                            Some(&i.to_string()),
                            &format!("{pointer}/{i}"),
                            v,
                            depth + 1,
                            open,
                        )
                    })
                    .collect()
            }),
        ),
        _ => return html! {},
    };
    let label = match &props.label {
        Some(k) => html! { <span class="tree__key">{ k.clone() }</span> },
        None if v.is_object() => html! { "facts" },
        None => html! {},
    };
    html! {
        <details class="tree__node" open={*expanded}>
            <summary {onclick}>{ label }<span class="tree__count">{ count }</span></summary>
            if let Some(children) = children {
                <div class="tree__children">{ children }</div>
            }
        </details>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn segments_resolve_awkward_keys() {
        let v = json!({"a/b": {"c~d": 1}});
        let pointer = format!("/{}/{}", segment("a/b"), segment("c~d"));
        assert_eq!(v.pointer(&pointer), Some(&json!(1)));
    }

    #[test]
    fn only_containers_of_containers_branch() {
        for (v, expected) in [
            (json!({"a": 1}), true),
            (json!({}), false),
            (json!([1, 2]), false),
            (json!([{"a": 1}]), true),
            (json!("x"), false),
        ] {
            assert_eq!(branch(&v), expected, "{v}");
        }
    }
}
