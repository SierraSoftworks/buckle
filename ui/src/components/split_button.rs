//! Every view's actions as one split button: the action an operator most
//! likely wants on its face, the rest behind the chevron, in sections when
//! they are of different kinds. Callers list the actions most likely first;
//! the first is the face.

use yew::prelude::*;

use super::icons;
use super::ui::Kind;

/// One action of a [`SplitButton`].
#[derive(Clone, PartialEq)]
pub struct MenuItem {
    pub label: AttrValue,
    /// A muted suffix that says where the action comes from (its module).
    pub aside: Option<AttrValue>,
    /// The menu section the action is listed under; a run of items with the
    /// same section is one group, headed by it when it has a name.
    pub section: Option<AttrValue>,
    /// How the face looks when this is the face; in the menu only `Danger`
    /// (red) and `Caution` (orange) show.
    pub kind: Kind,
    pub title: Option<AttrValue>,
    pub onclick: Callback<MouseEvent>,
}

impl MenuItem {
    pub fn new(label: impl Into<AttrValue>, onclick: Callback<MouseEvent>) -> Self {
        Self {
            label: label.into(),
            aside: None,
            section: None,
            kind: Kind::Default,
            title: None,
            onclick,
        }
    }

    pub fn kind(self, kind: Kind) -> Self {
        Self { kind, ..self }
    }

    pub fn danger(self) -> Self {
        self.kind(Kind::Danger)
    }

    pub fn aside(self, aside: impl Into<AttrValue>) -> Self {
        Self {
            aside: Some(aside.into()),
            ..self
        }
    }

    pub fn section(self, section: impl Into<AttrValue>) -> Self {
        Self {
            section: Some(section.into()),
            ..self
        }
    }

    pub fn title(self, title: impl Into<AttrValue>) -> Self {
        Self {
            title: Some(title.into()),
            ..self
        }
    }

    fn content(&self) -> Html {
        html! { <>
            { self.label.clone() }
            if let Some(aside) = &self.aside {
                { " " }<span class="split__aside">{ aside.clone() }</span>
            }
        </> }
    }
}

#[derive(Properties, PartialEq)]
pub struct SplitButtonProps {
    /// The actions, most likely first: the first is the face. Nothing
    /// renders when there are none, and no chevron when there is one.
    pub items: Vec<MenuItem>,
    /// What the chevron opens, for a screen reader.
    #[prop_or(AttrValue::Static("More actions"))]
    pub menu_label: AttrValue,
    #[prop_or_default]
    pub small: bool,
    /// Nothing calls for an action yet (a routine row): no face, only the
    /// chevron, and every action in its menu.
    #[prop_or_default]
    pub menu_only: bool,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

fn btn_kind(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Primary => Some("btn--primary"),
        Kind::Danger => Some("btn--danger"),
        Kind::Subtle => Some("btn--subtle"),
        Kind::Caution => Some("btn--caution"),
        Kind::Default => None,
    }
}

/// The menu's items in groups of one section each, in order.
fn groups(items: &[MenuItem]) -> Vec<(Option<AttrValue>, &[MenuItem])> {
    let mut out: Vec<(Option<AttrValue>, &[MenuItem])> = Vec::new();
    let mut start = 0;
    for i in 1..=items.len() {
        if i == items.len() || items[i].section != items[start].section {
            out.push((items[start].section.clone(), &items[start..i]));
            start = i;
        }
    }
    out
}

/// About the height of a menu, to decide whether it opens upwards.
const MENU_ROOM: f64 = 260.0;

/// Where an open menu goes: below the button's right edge, or above it near
/// the foot of the screen. Placed against the viewport, so that a scrolling
/// table does not clip it.
fn place(el: &web_sys::Element) -> String {
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
    if r.bottom() + MENU_ROOM > height && r.top() > MENU_ROOM {
        format!(
            "right: {right}px; bottom: {}px; top: auto",
            height - r.top() + 4.0
        )
    } else {
        format!("right: {right}px; top: {}px", r.bottom() + 4.0)
    }
}

#[function_component(SplitButton)]
pub fn split_button(props: &SplitButtonProps) -> Html {
    // Where the open menu sits; `None` while it is closed (and out of the DOM).
    let open = use_state(|| None::<String>);
    let node = use_node_ref();
    // A menu placed against the viewport follows its button as the page
    // scrolls or resizes.
    {
        let (open, node) = (open.clone(), node.clone());
        use_effect_with(open.is_some(), move |shown| {
            let listener = shown.then(|| {
                let follow = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                    if let Some(e) = node.cast::<web_sys::Element>() {
                        open.set(Some(place(&e)));
                    }
                });
                let w = web_sys::window();
                for event in ["scroll", "resize"] {
                    if let Some(w) = &w {
                        let _ = w.add_event_listener_with_callback_and_bool(
                            event,
                            wasm_bindgen::JsCast::unchecked_ref(follow.as_ref()),
                            true,
                        );
                    }
                }
                follow
            });
            move || {
                if let (Some(follow), Some(w)) = (listener, web_sys::window()) {
                    for event in ["scroll", "resize"] {
                        let _ = w.remove_event_listener_with_callback_and_bool(
                            event,
                            wasm_bindgen::JsCast::unchecked_ref(follow.as_ref()),
                            true,
                        );
                    }
                }
            }
        });
    }
    let Some((face, rest)) = props.items.split_first() else {
        return html! {};
    };
    let menu_items = if props.menu_only {
        &props.items[..]
    } else {
        rest
    };
    let toggle = {
        let (open, node) = (open.clone(), node.clone());
        Callback::from(move |_: MouseEvent| {
            open.set(match *open {
                Some(_) => None,
                None => Some(
                    node.cast::<web_sys::Element>()
                        .map(|e| place(&e))
                        .unwrap_or_default(),
                ),
            })
        })
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
                open.set(None);
            }
        })
    };
    let key = {
        let open = open.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                open.set(None);
            }
        })
    };
    // A row that opens on a click does not open for its actions.
    let contain = Callback::from(|e: MouseEvent| e.stop_propagation());
    let kind = btn_kind(face.kind);
    let small = props.small.then_some("btn--small");
    let menu = (*open).as_ref().map(|at| {
        html! {
            <div class="split__menu" role="menu" style={at.clone()}>
                { for groups(menu_items).into_iter().map(|(section, items)| html! {
                    <div class="split__group" role="group" aria-label={section.clone()}>
                        if let Some(s) = &section {
                            <div class="split__section" aria-hidden="true">{ s.clone() }</div>
                        }
                        { for items.iter().map(|item| {
                            let (open, onclick) = (open.clone(), item.onclick.clone());
                            html! {
                                <button type="button" role="menuitem" title={item.title.clone()}
                                    class={classes!("split__item", match item.kind {
                                        Kind::Danger => Some("split__item--danger"),
                                        Kind::Caution => Some("split__item--caution"),
                                        _ => None,
                                    })}
                                    onclick={Callback::from(move |e: MouseEvent| { open.set(None); onclick.emit(e); })}>
                                    { item.content() }
                                </button>
                            }
                        }) }
                    </div>
                }) }
            </div>
        }
    });
    html! {
        <div class={classes!("split", open.is_some().then_some("split--open"))} ref={node}
            onfocusout={blur} onkeydown={key} onclick={contain} data-testid={props.testid.clone()}>
            if props.menu_only {
                <button type="button" class={classes!("btn", "split__toggle", "split__toggle--alone", small)} onclick={toggle}
                    aria-haspopup="menu" aria-expanded={open.is_some().to_string()} aria-label={props.menu_label.clone()}
                    title={props.menu_label.clone()}>
                    { icons::chevron() }
                </button>
                { menu.unwrap_or_default() }
            } else {
                <button type="button" class={classes!("btn", "split__main", kind, small, rest.is_empty().then_some("split__main--alone"))}
                    title={face.title.clone()} onclick={face.onclick.clone()}>
                    { face.content() }
                </button>
                if !rest.is_empty() {
                    <button type="button" class={classes!("btn", "split__toggle", kind, small)} onclick={toggle}
                        aria-haspopup="menu" aria-expanded={open.is_some().to_string()} aria-label={props.menu_label.clone()}>
                        { icons::chevron() }
                    </button>
                    { menu.unwrap_or_default() }
                }
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &'static str, section: Option<&'static str>) -> MenuItem {
        let item = MenuItem::new(label, Callback::noop());
        match section {
            Some(s) => item.section(s),
            None => item,
        }
    }

    #[test]
    fn a_menu_groups_each_run_of_one_section() {
        let items = [
            item("Pause…", None),
            item("Reboot…", Some("Actions")),
            item("Restart", Some("Actions")),
            item("Forget…", None),
        ];
        let got: Vec<(Option<String>, Vec<String>)> = groups(&items)
            .into_iter()
            .map(|(s, items)| {
                (
                    s.map(|s| s.to_string()),
                    items.iter().map(|i| i.label.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (None, vec!["Pause…".to_string()]),
                (
                    Some("Actions".to_string()),
                    vec!["Reboot…".to_string(), "Restart".to_string()]
                ),
                (None, vec!["Forget…".to_string()]),
            ]
        );
        assert!(groups(&[]).is_empty());
    }
}
