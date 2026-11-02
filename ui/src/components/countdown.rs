//! A countdown that ticks by itself, so a bake timer does not re-render the
//! page around it every second.

use chrono::{DateTime, Utc};
use yew::prelude::*;

use crate::hooks::use_now;
use crate::logic::format::{countdown, timestamp};

#[derive(Properties, PartialEq)]
pub struct CountdownProps {
    pub until: DateTime<Utc>,
    #[prop_or_default]
    pub prefix: Option<AttrValue>,
    #[prop_or(AttrValue::from("done"))]
    pub done: AttrValue,
}

#[function_component(Countdown)]
pub fn view(props: &CountdownProps) -> Html {
    let now = use_now(1);
    let left = props.until.signed_duration_since(now).num_seconds();
    html! {
        <time class="countdown mono" datetime={props.until.to_rfc3339()} title={timestamp(props.until)}>
            if left > 0 {
                if let Some(p) = &props.prefix { { p.clone() }{ " " } }
                { countdown(left) }
            } else {
                { props.done.clone() }
            }
        </time>
    }
}
