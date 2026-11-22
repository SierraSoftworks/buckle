//! The app bar's bell: the server's attention list (`GET /attention`, the
//! same list webhooks are sent), grouped by what each item is about, in
//! yewi's notification panel. The list is only in the page while the panel
//! is open.

use buckle_api::LiveEvent;
use buckle_api::attention::{
    AttentionItem, AttentionList, AttentionSubject, AttentionTone, SubjectKind,
};
use chrono::{DateTime, Utc};
use yew::prelude::*;
use yewi::{NavTarget, NotificationItem, NotificationPanel, NotificationReason, attrs};

use crate::api;
use crate::app::Route;
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::format::short_duration;
use crate::logic::tone::Tone;

#[function_component(AttentionBell)]
pub fn attention_bell() -> Html {
    let now = use_now(30);
    let list = use_fetch((), |_| api::attention());
    let Some(list) = use_live_value(&list, apply) else {
        return html! {};
    };
    let groups = group(&list.items);
    let worst = groups
        .first()
        .map(|(_, items)| tone(items[0].tone))
        .unwrap_or(Tone::Info);
    let label = match groups.len() {
        0 => "Nothing needs attention".to_string(),
        1 => "1 item needs attention".to_string(),
        n => format!("{n} items need attention"),
    };
    html! {
        <NotificationPanel title="Needs attention" {label} count={groups.len() as u32} tone={worst.yi()}
            description={html! { "Unhealthy hosts, checks and workloads, registrations, offline agents, version skew, stale plans, paused rollouts, skipped schedules, refused identities and failing webhooks" }}
            empty={html! { "Every host and workload is healthy, and every agent registered, connected, current and on a plan its phase approved." }}
            attrs={attrs! { "data-testid" => "attention-bell" }}>
            { for groups.into_iter().map(|(subject, items)| {
                let worst = tone(items[0].tone);
                let href = route(subject).map(NavTarget::route);
                html! {
                    <NotificationItem key={format!("{:?}/{}", subject.kind, subject.id)} tone={worst.yi()}
                        title={html! { <span class="mono">{ title(subject) }</span> }}
                        {href}
                        attrs={attrs! { "data-host" => subject.id.clone() }}>
                        { for items.iter().map(|i| html! {
                            <NotificationReason tone={tone(i.tone).yi()}>{ describe(i, now) }</NotificationReason>
                        }) }
                    </NotificationItem>
                }
            }) }
        </NotificationPanel>
    }
}

/// The server sends the whole list whenever it changes.
fn apply(list: &mut AttentionList, event: &LiveEvent) {
    if let LiveEvent::Attention { list: next } = event {
        list.clone_from(next);
    }
}

/// The items by subject, in the server's order (worst first).
fn group(items: &[AttentionItem]) -> Vec<(&AttentionSubject, Vec<&AttentionItem>)> {
    let mut out: Vec<(&AttentionSubject, Vec<&AttentionItem>)> = Vec::new();
    for i in items {
        match out.iter_mut().find(|(s, _)| *s == &i.subject) {
            Some((_, list)) => list.push(i),
            None => out.push((&i.subject, vec![i])),
        }
    }
    out
}

fn tone(t: AttentionTone) -> Tone {
    match t {
        AttentionTone::Danger => Tone::Danger,
        AttentionTone::Warn => Tone::Warn,
        AttentionTone::Info => Tone::Info,
    }
}

fn title(s: &AttentionSubject) -> String {
    match s.kind {
        SubjectKind::Rollout => buckle_api::rollout_name(&s.id)
            .map(|n| format!("rollout {n}"))
            .unwrap_or_else(|| format!("rollout #{}", s.id)),
        SubjectKind::Schedule => format!("schedule {}", s.id),
        SubjectKind::Webhook => format!("webhook {}", s.id),
        SubjectKind::Host | SubjectKind::Cluster => s.id.clone(),
    }
}

fn route(s: &AttentionSubject) -> Option<Route> {
    let id = s.id.clone();
    Some(match s.kind {
        SubjectKind::Host => Route::Host { id },
        SubjectKind::Cluster => Route::Cluster { id },
        SubjectKind::Rollout => Route::Rollout { id },
        SubjectKind::Schedule => Route::Schedule { name: id },
        SubjectKind::Webhook => return None,
    })
}

/// The item's summary, with how long it has lasted when that is known.
fn describe(i: &AttentionItem, now: DateTime<Utc>) -> String {
    match i.since {
        Some(at) if at <= now => format!(
            "{} · {}",
            i.summary,
            short_duration(now.signed_duration_since(at).num_seconds())
        ),
        _ => i.summary.clone(),
    }
}
