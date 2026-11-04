//! A button with a menu beside it: the one action a view expects on its
//! face, the rarer ones behind the chevron. [`ActionMenu`] is the same menu
//! behind a lone "⋮" button, for a row's actions.

use yew::prelude::*;

use super::icons;
use super::ui::Kind;

#[derive(Clone, PartialEq)]
pub struct MenuItem {
    pub label: AttrValue,
    pub danger: bool,
    pub onclick: Callback<MouseEvent>,
}

#[derive(Properties, PartialEq)]
pub struct SplitButtonProps {
    pub label: AttrValue,
    pub onclick: Callback<MouseEvent>,
    #[prop_or_default]
    pub kind: Kind,
    #[prop_or_default]
    pub items: Vec<MenuItem>,
    /// What the chevron opens, for a screen reader.
    #[prop_or(AttrValue::Static("More actions"))]
    pub menu_label: AttrValue,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// An open/closed menu that closes when focus leaves `node` or on Escape.
struct Menu {
    open: UseStateHandle<bool>,
    node: NodeRef,
    toggle: Callback<MouseEvent>,
    blur: Callback<FocusEvent>,
    key: Callback<KeyboardEvent>,
}

#[hook]
fn use_menu() -> Menu {
    let open = use_state(|| false);
    let node = use_node_ref();
    let toggle = {
        let open = open.clone();
        Callback::from(move |_: MouseEvent| open.set(!*open))
    };
    // Closed when focus leaves it (a click elsewhere) or on Escape.
    let blur = {
        let (open, node) = (open.clone(), node.clone());
        Callback::from(move |e: FocusEvent| {
            let inside = e
                .related_target()
                .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Node>(t).ok())
                .zip(node.cast::<web_sys::Node>())
                .is_some_and(|(t, root)| root.contains(Some(&t)));
            if !inside {
                open.set(false);
            }
        })
    };
    let key = {
        let open = open.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                open.set(false);
            }
        })
    };
    Menu {
        open,
        node,
        toggle,
        blur,
        key,
    }
}

fn menu_items(items: &[MenuItem], open: &UseStateHandle<bool>, style: Option<&str>) -> Html {
    html! {
        <div class="split__menu" role="menu" style={style.map(str::to_owned)}>
            { for items.iter().map(|item| {
                let (open, onclick) = (open.clone(), item.onclick.clone());
                html! {
                    <button type="button" role="menuitem"
                        class={classes!("split__item", item.danger.then_some("split__item--danger"))}
                        onclick={Callback::from(move |e: MouseEvent| { open.set(false); onclick.emit(e); })}>
                        { item.label.clone() }
                    </button>
                }
            }) }
        </div>
    }
}

#[function_component(SplitButton)]
pub fn split_button(props: &SplitButtonProps) -> Html {
    let Menu {
        open,
        node,
        toggle,
        blur,
        key,
    } = use_menu();
    let kind = match props.kind {
        Kind::Primary => Some("btn--primary"),
        Kind::Danger => Some("btn--danger"),
        Kind::Subtle => Some("btn--subtle"),
        Kind::Caution => Some("btn--caution"),
        Kind::Default => None,
    };
    html! {
        <div class={classes!("split", (*open).then_some("split--open"))} ref={node}
            onfocusout={blur} onkeydown={key} data-testid={props.testid.clone()}>
            <button type="button" class={classes!("btn", "split__main", kind)} onclick={props.onclick.clone()}>
                { props.label.clone() }
            </button>
            if !props.items.is_empty() {
                <button type="button" class={classes!("btn", "split__toggle", kind)} onclick={toggle}
                    aria-haspopup="menu" aria-expanded={open.to_string()} aria-label={props.menu_label.clone()}>
                    { icons::chevron() }
                </button>
                if *open { { menu_items(&props.items, &open, None) } }
            }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct ActionMenuProps {
    pub items: Vec<MenuItem>,
    /// What the button opens, for a screen reader.
    pub label: AttrValue,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// A row's actions behind a "⋮" button; nothing when there are none. The
/// menu is placed against the viewport (below the button, or above it near
/// the foot of the screen), so that a scrolling table does not clip it.
#[function_component(ActionMenu)]
pub fn action_menu(props: &ActionMenuProps) -> Html {
    let Menu {
        open,
        node,
        toggle,
        blur,
        key,
    } = use_menu();
    let at = use_state(String::new);
    if props.items.is_empty() {
        return html! {};
    }
    let toggle = {
        let (at, node) = (at.clone(), node.clone());
        Callback::from(move |e: MouseEvent| {
            if let Some(el) = node.cast::<web_sys::Element>() {
                let r = el.get_bounding_client_rect();
                let size = |f: fn(&web_sys::Window) -> Result<wasm_bindgen::JsValue, _>| {
                    web_sys::window()
                        .and_then(|w| f(&w).ok())
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0)
                };
                let (width, height) = (
                    size(web_sys::Window::inner_width),
                    size(web_sys::Window::inner_height),
                );
                let right = (width - r.right()).max(8.0);
                at.set(if r.bottom() + MENU_ROOM > height && r.top() > MENU_ROOM {
                    format!(
                        "position: fixed; right: {right}px; bottom: {}px; top: auto",
                        height - r.top() + 4.0
                    )
                } else {
                    format!(
                        "position: fixed; right: {right}px; top: {}px",
                        r.bottom() + 4.0
                    )
                });
            }
            toggle.emit(e);
        })
    };
    html! {
        <div class={classes!("split", "split--kebab", (*open).then_some("split--open"))} ref={node}
            onfocusout={blur} onkeydown={key} data-testid={props.testid.clone()}>
            <button type="button" class="icon-btn" onclick={toggle} title={props.label.clone()}
                aria-haspopup="menu" aria-expanded={open.to_string()} aria-label={props.label.clone()}>
                { icons::kebab() }
            </button>
            if *open { { menu_items(&props.items, &open, Some(&*at)) } }
        </div>
    }
}

/// About the height of a row menu, to decide whether it opens upwards.
const MENU_ROOM: f64 = 220.0;
