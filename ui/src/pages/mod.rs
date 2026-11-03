//! One module per page of design §12.

pub mod audit;
pub mod host;
pub mod hosts;
pub mod overview;
pub mod revisions;
pub mod rollout;
pub mod rollouts;

use yew::prelude::*;

use crate::app::Route;
use crate::components::{Empty, RouteLink};

#[function_component(NotFound)]
pub fn not_found() -> Html {
    html! {
        <div class="page">
            <Empty title="Nothing here" message="This address is not a page of Buckle.">
                <RouteLink to={Route::Overview} classes="btn">{ "Go to the overview" }</RouteLink>
            </Empty>
        </div>
    }
}
