//! The deployment's tag (`web.environment`) beside the Buckle name, and the
//! tab title that carries it.

use yew::prelude::*;

use crate::app::use_auth;
use crate::logic::environment::{page_title, tag_style};

/// Names the tab after the environment, so a production tab says so.
pub fn title(env: Option<&buckle_api::Environment>) {
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        doc.set_title(&page_title(env));
    }
}

#[function_component(EnvironmentTag)]
pub fn environment_tag() -> Html {
    let Some(env) = use_auth().environment else {
        return html! {};
    };
    let mut attrs = vec![
        ("data-testid", AttrValue::from("environment")),
        (
            "title",
            AttrValue::from(format!("This is the {} Buckle.", env.name)),
        ),
    ];
    if let Some(style) = tag_style(&env) {
        attrs.push(("style", style.into()));
    }
    html! {
        <yewi::Tag class="environment-tag" attrs={yewi::Attrs::new(attrs)}>
            { env.name.clone() }
        </yewi::Tag>
    }
}
