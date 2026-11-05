//! The app bar's bell: how many hosts and clusters need attention
//! (registrations, offline agents, version skew, failing or held health,
//! failing workloads, and the live rollout's failed, held or stale-plan
//! hosts), with the list in a dropdown. The list is only in the
//! page while the dropdown is open.

use buckle_api::{Fleet, RolloutDetail};
use chrono::{DateTime, Utc};
use yew::prelude::*;
use yew_router::prelude::*;

use super::{HostLink, icons};
use crate::api;
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{Concern, attention};
use crate::logic::live::{apply_fleet, apply_rollout};
use crate::logic::tone::{Tone, rollout_finished};

#[function_component(AttentionBell)]
pub fn attention_bell() -> Html {
    let now = use_now(30);
    let route = use_route::<crate::app::Route>();
    let fleet = use_fetch((), |_| api::fleet());
    let fleet = use_live_value(&fleet, apply_fleet);
    // The live rollout's hosts; nothing to fetch while none runs.
    let active = fleet.as_ref().and_then(|f| f.active_rollout.clone());
    let rollout = use_fetch(active.clone(), |id| async move {
        match id {
            Some(id) => api::rollout(&id).await.map(Some),
            None => Ok(None),
        }
    });
    let rollout = use_live_value(&rollout, |d: &mut Option<RolloutDetail>, e| {
        if let Some(d) = d {
            apply_rollout(d, e)
        }
    })
    .flatten()
    .filter(|d| Some(&d.summary.id) == active.as_ref() && !rollout_finished(d.summary.state));

    let open = use_state(|| false);
    let node = use_node_ref();
    // Following a link from the list closes it.
    {
        let open = open.clone();
        use_effect_with(route, move |_| {
            open.set(false);
            || ()
        });
    }
    let toggle = {
        let open = open.clone();
        Callback::from(move |_: MouseEvent| open.set(!*open))
    };
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

    let Some(f) = fleet else {
        return html! {};
    };
    let list = concerns(&f, rollout.as_ref(), now);
    let worst = worst(list.iter().flat_map(|(_, c)| c));
    let label = match list.len() {
        0 => "Nothing needs attention".to_string(),
        1 => "1 target needs attention".to_string(),
        n => format!("{n} targets need attention"),
    };
    html! {
        <div class={classes!("bell", (*open).then_some("bell--open"))} ref={node}
            onfocusout={blur} onkeydown={key} data-testid="attention-bell">
            <button type="button" class="icon-btn bell__button" onclick={toggle}
                aria-label={label.clone()} title={label.clone()} aria-haspopup="true" aria-expanded={(*open).to_string()}>
                { icons::bell() }
                if !list.is_empty() {
                    <span class={classes!("bell__count", worst.class())} aria-hidden="true">{ list.len() }</span>
                }
            </button>
            if *open {
                <div class="bell__panel" role="dialog" aria-label="Needs attention" tabindex="-1">
                    <div class="bell__head">
                        <span class="bell__title">{ "Needs attention" }</span>
                        <span class="muted small">{ "Unhealthy hosts and workloads, registrations, offline agents, version skew and stale plans" }</span>
                    </div>
                    if list.is_empty() {
                        <p class="bell__empty muted">{ "Every host and workload is healthy, and every agent registered, connected, current and on a plan its phase approved." }</p>
                    } else {
                        <ul class="attention">
                            { for list.into_iter().map(|(host, concerns)| {
                                let summary = f.hosts.iter().chain(&f.clusters).find(|h| h.id == host).cloned();
                                html! {
                                    <li class={classes!("attention__item", worst_of(&concerns).class())} data-host={host.clone()}>
                                        <span class="attention__mark" aria-hidden="true" />
                                        <HostLink id={host.clone()} host={summary} {now} classes={classes!("attention__host", "mono")} />
                                        <span class="attention__why">
                                            { for concerns.iter().map(|c| html! {
                                                <span class={classes!("attention__concern", c.tone().class())}>{ c.describe(now) }</span>
                                            }) }
                                        </span>
                                    </li>
                                }
                            }) }
                        </ul>
                    }
                </div>
            }
        </div>
    }
}

fn concerns(
    f: &Fleet,
    rollout: Option<&RolloutDetail>,
    now: DateTime<Utc>,
) -> Vec<(String, Vec<Concern>)> {
    attention(
        &f.hosts,
        &f.clusters,
        f.desired_agent_version
            .as_deref()
            .or(f.server_version.as_deref()),
        rollout,
        now,
    )
}

/// The most severe tone among `concerns`.
fn worst<'a>(concerns: impl Iterator<Item = &'a Concern> + Clone) -> Tone {
    [Tone::Danger, Tone::Warn]
        .into_iter()
        .find(|t| concerns.clone().any(|c| c.tone() == *t))
        .unwrap_or(Tone::Info)
}

fn worst_of(concerns: &[Concern]) -> Tone {
    worst(concerns.iter())
}
