//! A target's blueprint (design §12): what Buckle manages there, a
//! module's resources drawn like the run report's steps, each with its
//! state against drift and the latest run, and whether removal cleans it up.

use buckle_api::{DriftItem, ResourceSummary, RunReport};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::diff::secret_text;
use crate::logic::blueprint::{Resource, blueprint};
use crate::logic::format::{commit, relative, timestamp};

/// One module's resources as the blueprint lists them, with what `run`
/// did to each and any drift; empty when Buckle manages none of them.
pub fn resource_steps(
    resources: &[ResourceSummary],
    drift: &[DriftItem],
    run: Option<&RunReport>,
    now: DateTime<Utc>,
) -> Html {
    let modules = blueprint(resources, drift, run);
    html! {
        <ol class="run-steps">
            { for modules.iter().flat_map(|m| m.resources.iter()).map(|r| resource_row(r, now)) }
        </ol>
    }
}

fn resource_row(r: &Resource, now: DateTime<Utc>) -> Html {
    let slug = r.state.slug();
    html! {
        <li class={classes!("run-step", format!("run-step--{slug}"))}
            data-resource={format!("{}[{}]", r.kind, r.key)} data-state={slug}>
            <span class={classes!("run-step__glyph", r.state.tone().class())} aria-hidden="true">{ r.state.glyph() }</span>
            <div class="run-step__main">
                <div class="run-step__head">
                    <span class="badge run-step__kind">{ r.kind.clone() }</span>
                    <span class="run-step__key mono">{ r.key.clone() }</span>
                    if r.cleanup {
                        <span class="badge badge--warn" title="Deleted from the host if the revision drops it">{ "cleanup" }</span>
                    } else {
                        <span class="badge" title="Left on the host, only forgotten, if the revision drops it">{ "forget only" }</span>
                    }
                    <span class="run-step__state">{ r.state.label() }</span>
                </div>
                if let Some(note) = &r.note { <p class="run-step__summary">{ secret_text(note) }</p> }
            </div>
            <span class="run-step__duration">
                if let Some((rev, at)) = &r.applied {
                    <span class="mono" title={rev.clone()}>{ commit(rev) }</span>{ " · " }
                    <span title={timestamp(*at)}>{ relative(*at, now) }</span>
                }
            </span>
        </li>
    }
}
