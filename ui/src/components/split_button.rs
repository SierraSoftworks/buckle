//! A button with a menu beside it: the one action a view expects on its
//! face, the rarer ones behind the chevron.

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

#[function_component(SplitButton)]
pub fn split_button(props: &SplitButtonProps) -> Html {
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
    let kind = match props.kind {
        Kind::Primary => Some("btn--primary"),
        Kind::Danger => Some("btn--danger"),
        Kind::Subtle => Some("btn--subtle"),
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
                if *open {
                    <div class="split__menu" role="menu">
                        { for props.items.iter().map(|item| {
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
        </div>
    }
}
