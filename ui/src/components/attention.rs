//! The app bar's bell: how many hosts and clusters need attention
//! (registrations, offline agents, version skew, failing or held health,
//! failing workloads, and the live rollout's failed, held or stale-plan
//! hosts), with the list in yewi's notification panel. The list is only
//! in the page while the panel is open.

use buckle_api::{Fleet, RolloutDetail};
use chrono::{DateTime, Utc};
use yew::prelude::*;
use yewi::{NavTarget, NotificationItem, NotificationPanel, NotificationReason, attrs};

use crate::api;
use crate::app::Route;
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{Concern, attention};
use crate::logic::live::{apply_fleet, apply_rollout};
use crate::logic::tone::{Tone, rollout_finished};

#[function_component(AttentionBell)]
pub fn attention_bell() -> Html {
    let now = use_now(30);
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
        <NotificationPanel title="Needs attention" {label} count={list.len() as u32} tone={worst.yi()}
            description={html! { "Unhealthy hosts and workloads, registrations, offline agents, version skew and stale plans" }}
            empty={html! { "Every host and workload is healthy, and every agent registered, connected, current and on a plan its phase approved." }}
            attrs={attrs! { "data-testid" => "attention-bell" }}>
            { for list.into_iter().map(|(host, concerns)| {
                let kind = f.hosts.iter().chain(&f.clusters).find(|h| h.id == host).map(|h| h.kind).unwrap_or_default();
                html! {
                    <NotificationItem key={host.clone()} tone={worst_of(&concerns).yi()}
                        title={html! { <span class="mono">{ host.clone() }</span> }}
                        href={NavTarget::route(Route::target(&host, kind))}
                        attrs={attrs! { "data-host" => host.clone() }}>
                        { for concerns.iter().map(|c| html! {
                            <NotificationReason tone={c.tone().yi()}>{ c.describe(now) }</NotificationReason>
                        }) }
                    </NotificationItem>
                }
            }) }
        </NotificationPanel>
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
