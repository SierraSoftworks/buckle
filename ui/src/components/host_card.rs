//! A host's name as a link that, hovered or focused, summarises the host:
//! its connection, health and status as pills, then what it runs. The same
//! card wherever a host is named, so hovering answers "how is it?" and a
//! click opens the host for everything else. A cluster's card tallies its
//! workloads and names what it is deploying and who speaks for it.

use buckle_api::{Connection, HealthSummary, HostStatus, HostSummary, UpgradeState};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::link::RouteLink;
use super::ui::{HoverTip, Pill};
use crate::app::Route;
use crate::logic::clusters::deploying_text;
use crate::logic::format::{commit, count, relative, relative_opt};
use crate::logic::tone::{Tone, health_label, health_tone, host_status_label, host_status_tone};

#[derive(Properties, PartialEq)]
pub struct HostLinkProps {
    pub id: AttrValue,
    /// The host as the fleet last reported it; without it the link has no card.
    #[prop_or_default]
    pub host: Option<HostSummary>,
    pub now: DateTime<Utc>,
    #[prop_or(classes!("mono", "strong"))]
    pub classes: Classes,
}

#[function_component(HostLink)]
pub fn host_link(props: &HostLinkProps) -> Html {
    let tip = props
        .host
        .as_ref()
        .map(|h| host_card(h, props.now))
        .unwrap_or_default();
    let kind = props.host.as_ref().map(|h| h.kind).unwrap_or_default();
    html! {
        <HoverTip link=true {tip}>
            <RouteLink to={Route::target(&props.id, kind)} classes={props.classes.clone()}>
                { props.id.clone() }
            </RouteLink>
        </HoverTip>
    }
}

/// The pills that say how a host stands: its health (or that it is
/// offline), then only what is out of the ordinary (paused, pending, drift).
pub fn host_state_pills(h: &HostSummary) -> Html {
    let offline = h.connection == Connection::Offline;
    html! { <>
        // A cluster target has no agent of its own: its badge replaces the
        // connection, and health shows once something has read it.
        if let Some(c) = &h.cluster {
            <Pill tone={Tone::Info} label={format!("{} cluster", c.spec.kind)}
                title={if c.declared { "a cluster target declared in buckle.yaml" } else { "a cluster target created through the API" }} />
            if h.health != HealthSummary::Unknown {
                <Pill tone={health_tone(h.health)} label={health_label(h.health)} />
            }
        } else if offline {
            <Pill tone={Tone::Neutral} label="offline" title={format!("last reading: {}", health_label(h.health))} />
        } else {
            <Pill tone={health_tone(h.health)} label={health_label(h.health)} />
        }
        if h.status != HostStatus::Active {
            <Pill tone={host_status_tone(h.status)} label={host_status_label(h.status)} />
        }
        if h.drifted { <Pill tone={Tone::Warn} label="drifted" /> }
    </> }
}

fn host_card(h: &HostSummary, now: DateTime<Utc>) -> Html {
    if let Some(c) = &h.cluster {
        return cluster_card(h, c, now);
    }
    let platform = [h.platform.as_deref(), h.arch.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let upgrade = h.upgrade.as_ref().map(|u| match u.state {
        UpgradeState::Requested => format!(", upgrading to {}", u.version),
        UpgradeState::Failed => format!(", upgrade to {} failed", u.version),
    });
    html! {
        <span class="host-card" data-testid="host-card">
            <span class="host-card__head">
                <strong class="mono">{ h.id.clone() }</strong>
                { host_state_pills(h) }
            </span>
            <span class="host-card__facts">
                if !platform.is_empty() { <span>{ platform }</span> }
                if let Some(v) = &h.agent_version {
                    <span>{ format!("agent {v}{}", upgrade.unwrap_or_default()) }</span>
                }
                if let Some(r) = &h.applied_revision {
                    <span>{ "revision " }<span class="mono">{ commit(r) }</span></span>
                }
                if !h.groups.is_empty() { <span>{ format!("groups {}", h.groups.join(", ")) }</span> }
                <span class="muted">
                    { format!("health read {} · seen {}", relative_opt(h.health_at, now), h.last_seen.map(|s| relative(s, now)).unwrap_or_else(|| "never".into())) }
                </span>
            </span>
        </span>
    }
}

fn cluster_card(h: &HostSummary, c: &buckle_api::ClusterInfo, now: DateTime<Utc>) -> Html {
    let status = h.cluster_status.clone().unwrap_or_default();
    html! {
        <span class="host-card" data-testid="host-card">
            <span class="host-card__head">
                <strong class="mono">{ h.id.clone() }</strong>
                { host_state_pills(h) }
            </span>
            <span class="host-card__facts">
                <span>{ count(status.workloads as usize, "workload", "workloads") }
                    if !status.failing.is_empty() {
                        { " · " }<span class="text-danger">{ format!("{} failing", status.failing.join(", ")) }</span>
                    }
                </span>
                if !status.deploying.is_empty() {
                    <span>{ format!("deploying {}", deploying_text(&status.deploying)) }</span>
                }
                <span>{ match &status.executor {
                    Some(e) => html! { <>{ "executor " }<span class="mono">{ e.clone() }</span></> },
                    None => html! { <span class="muted">{ "no executor yet" }</span> },
                } }</span>
                if let Some(api) = c.spec.api.as_deref() { <span class="mono">{ api.to_owned() }</span> }
                if let Some(r) = &h.applied_revision {
                    <span>{ "revision " }<span class="mono">{ commit(r) }</span></span>
                }
                if !h.groups.is_empty() { <span>{ format!("groups {}", h.groups.join(", ")) }</span> }
                <span class="muted">{ format!("health read {}", relative_opt(h.health_at, now)) }</span>
            </span>
        </span>
    }
}
