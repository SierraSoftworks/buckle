//! One module per page of design §12.

pub mod add_agent;
pub mod add_cluster;
pub mod audit;
pub mod cluster;
pub mod clusters;
pub mod host;
pub mod hosts;
mod modules_card;
pub mod overview;
pub mod rollout;
pub mod rollouts;
pub mod workload;
pub mod workloads;

use yew::prelude::*;

use crate::app::Route;
use crate::components::Empty;

#[function_component(NotFound)]
pub fn not_found() -> Html {
    html! {
        <div class="page">
            <Empty title="Nothing here" message="This address is not a page of Buckle.">
                <yewi::Button href={yewi::NavTarget::route(Route::Overview)}>{ "Go to the overview" }</yewi::Button>
            </Empty>
        </div>
    }
}
