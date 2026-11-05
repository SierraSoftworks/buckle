//! An in-app link: a real `<a href>` (so it can be opened in a new tab and is
//! reached with the keyboard) that navigates without reloading the page, so
//! the live store and demo mode survive the journey.

use yew::prelude::*;
use yew_router::prelude::*;

use crate::app::Route;
use crate::util::enc;

#[derive(Properties, PartialEq)]
pub struct RouteLinkProps {
    pub to: Route,
    /// Query parameters, e.g. a hosts-table filter.
    #[prop_or_default]
    pub query: Vec<(String, String)>,
    #[prop_or_default]
    pub classes: Classes,
    #[prop_or_default]
    pub label: Option<AttrValue>,
    #[prop_or_default]
    pub title: Option<AttrValue>,
    #[prop_or_default]
    pub data_host: Option<AttrValue>,
    #[prop_or_default]
    pub data_state: Option<AttrValue>,
    #[prop_or_default]
    pub data_workload: Option<AttrValue>,
    #[prop_or_default]
    pub current: bool,
    #[prop_or_default]
    pub children: Html,
}

#[function_component(RouteLink)]
pub fn route_link(props: &RouteLinkProps) -> Html {
    let navigator = use_navigator();
    let onclick = {
        let (to, query) = (props.to.clone(), props.query.clone());
        Callback::from(move |e: MouseEvent| {
            // Leave a modified click (new tab, new window) to the browser.
            if e.button() != 0 || e.ctrl_key() || e.meta_key() || e.shift_key() || e.alt_key() {
                return;
            }
            if let Some(n) = &navigator {
                e.prevent_default();
                if query.is_empty() {
                    n.push(&to);
                } else {
                    let _ = n.push_with_query(&to, &query);
                }
            }
        })
    };
    let href = if props.query.is_empty() {
        props.to.to_path()
    } else {
        let q: Vec<String> = props
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect();
        format!("{}?{}", props.to.to_path(), q.join("&"))
    };
    html! {
        <a {href} class={props.classes.clone()} {onclick}
            aria-label={props.label.clone()} title={props.title.clone()}
            data-host={props.data_host.clone()} data-state={props.data_state.clone()}
            data-workload={props.data_workload.clone()}
            aria-current={props.current.then_some("page")}>
            { props.children.clone() }
        </a>
    }
}
