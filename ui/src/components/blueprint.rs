//! A target's blueprint (design §12): what Buckle manages there, a
//! module's resources drawn like the run report's steps, each with its
//! state against drift and the latest run, and whether removal cleans it up.

use crate::components::ui::Badge;
use crate::logic::tone::Tone;
use buckle_api::{ClusterSpec, DriftItem, ResourceSummary, RunReport};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::diff::secret_text;
use super::icons;
use crate::logic::blueprint::{Resource, blueprint};
use crate::logic::clusters::resource_url;
use crate::logic::format::{commit, relative, timestamp};

/// One module's resources as the blueprint lists them, with what `run`
/// did to each and any drift; empty when Buckle manages none of them. On a
/// cluster (`spec`), a resource its UI shows links there.
pub fn resource_steps(
    resources: &[ResourceSummary],
    drift: &[DriftItem],
    run: Option<&RunReport>,
    spec: Option<&ClusterSpec>,
    now: DateTime<Utc>,
) -> Html {
    let modules = blueprint(resources, drift, run);
    html! {
        <ol class="run-steps">
            { for modules.iter().flat_map(|m| m.resources.iter()).map(|r| resource_row(r, spec, now)) }
        </ol>
    }
}

fn resource_row(r: &Resource, spec: Option<&ClusterSpec>, now: DateTime<Utc>) -> Html {
    let slug = r.state.slug();
    let url = resource_url(spec, &r.kind, &r.key);
    html! {
        <li class={classes!("run-step", format!("run-step--{slug}"))}
            data-resource={format!("{}[{}]", r.kind, r.key)} data-state={slug}>
            <span class={classes!("run-step__glyph", r.state.tone().class())} aria-hidden="true">{ r.state.glyph() }</span>
            <div class="run-step__main">
                <div class="run-step__head">
                    <Badge class="run-step__kind">{ r.kind.clone() }</Badge>
                    <span class="run-step__key mono">{ r.key.clone() }</span>
                    if let (Some(url), Some(spec)) = (url, spec) {
                        <a href={url} target="_blank" rel="noopener noreferrer"
                            aria-label={format!("Open {} in the cluster's UI", r.key)}>{ ui_name(spec) }{ icons::external() }</a>
                    }
                    if r.cleanup {
                        <Badge tone={Tone::Warn} title="Deleted from the host if the revision drops it">{ "cleanup" }</Badge>
                    } else {
                        <Badge title="Left on the host, only forgotten, if the revision drops it">{ "forget only" }</Badge>
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

/// What the cluster's own UI is called on a link into it.
fn ui_name(spec: &ClusterSpec) -> &'static str {
    match spec.kind {
        buckle_api::ClusterKind::Nomad => "Nomad",
        buckle_api::ClusterKind::Kubernetes => "Kubernetes",
    }
}
