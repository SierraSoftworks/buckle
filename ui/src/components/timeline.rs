//! The Rollouts timeline ([`crate::logic::timeline`]): each commit as its
//! card, and between them each operator's action as one line on the rail,
//! marked with a diamond and a bolt in the action colour: who ran what,
//! where, how it went and how long it took, with `module/action` where a
//! commit has its hash. A line opens its rollout, or the target it ran on.

use buckle_api::{ActionRun, RevisionSummary, RolloutState, RolloutSummary};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::avatar::{Avatar, split_author};
use super::changes::Took;
use super::icons;
use super::meter::rollout_segments;
use super::revision::{RevisionAsk, revision_item};
use super::{Meter, Pill, RouteLink};
use crate::app::Route;
use crate::logic::format::{
    count, relative, rollout_label, rollout_note, short_duration, timestamp,
};
use crate::logic::run::{run_label, run_tone};
use crate::logic::timeline::{Entry, Show, entries};
use crate::logic::tone::{rollout_finished, rollout_state_label, rollout_state_tone};

#[derive(Properties, PartialEq)]
pub struct TimelineProps {
    pub revisions: Vec<RevisionSummary>,
    pub rollouts: Vec<RolloutSummary>,
    pub runs: Vec<ActionRun>,
    pub show: Show,
    /// Leave out anything older (see [`entries`]).
    #[prop_or_default]
    pub since: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
    pub open: Callback<RevisionAsk>,
}

#[function_component(Timeline)]
pub fn timeline(props: &TimelineProps) -> Html {
    let stable_at = props
        .revisions
        .iter()
        .find(|r| r.is_stable)
        .map(|r| r.created_at);
    let list = entries(
        &props.revisions,
        &props.rollouts,
        &props.runs,
        props.show,
        props.since,
    );
    html! {
        <ol class="gitlog timeline">
            { for list.iter().map(|e| match e {
                Entry::Commit(r, x) => revision_item(r, *x, stable_at, props.now, &props.open),
                Entry::Rollout(x) => action_rollout(x, props.now),
                Entry::Run(r) => action_run(r, props.now),
            }) }
        </ol>
    }
}

/// An action's line on the rail: diamond, bolt, who ran it, then `body`;
/// `aside` at the right, where a commit has its hash.
fn line(key: String, kind: &'static str, by: Option<&str>, body: Html, aside: Html) -> Html {
    let person = by.map(|by| {
        let (name, email) = split_author(by);
        let title = email.map(|e| format!("{name} <{e}>"));
        html! { <><span class="commit__author" {title}><Avatar author={by.to_string()} />{ name.to_string() }</span><span class="muted">{ "ran" }</span></> }
    });
    html! {
        <li {key} class="gitlog__item gitlog__item--action" data-action={kind}>
            <span class="gitlog__diamond" aria-hidden="true" />
            <div class="action-line commit commit--link">
                <div class="action-line__main">
                    <span class="action-line__icon" title="An operator's action">{ icons::bolt() }</span>
                    { person.unwrap_or_default() }
                    { body }
                </div>
                <div class="commit__aside">{ aside }</div>
            </div>
        </li>
    }
}

fn action_key(module: &str, action: &str) -> Html {
    html! { <span class="action-line__key mono" title="module/action">{ format!("{module}/{action}") }</span> }
}

fn when(at: DateTime<Utc>, now: DateTime<Utc>) -> Html {
    html! { <span class="muted" title={timestamp(at)}>{ relative(at, now) }</span> }
}

/// An action rolled out across the fleet: its rollout's name in the
/// rollout's colour, then the action, its targets, state and progress.
fn action_rollout(x: &RolloutSummary, now: DateTime<Utc>) -> Html {
    let Some(a) = &x.action else {
        return html! {};
    };
    let targets = count(x.hosts_total as usize, "target", "targets");
    let scope = match &a.selector {
        Some(s) => {
            html! { <>{ format!("on {targets} matching ") }<span class="mono">{ s.clone() }</span></> }
        }
        None => html! { { format!("on {targets}") } },
    };
    let finished = rollout_finished(x.state);
    let took = x
        .finished_at
        .filter(|_| finished)
        .map(|f| short_duration(f.signed_duration_since(x.started_at).num_seconds()));
    let phase = x.current_phase.as_ref().filter(|_| !finished);
    let review = (x.state == RolloutState::AwaitingApproval).then(|| {
        html! {
            <RouteLink to={Route::rollout(&x.id)} classes="btn btn--small btn--info"
                title="This rollout is waiting for an approval">{ "Review…" }</RouteLink>
        }
    });
    let body = html! { <>
        <RouteLink to={Route::rollout(&x.id)} classes="commit__subject" title={format!("Open rollout {}", rollout_label(&x.id))}>
            <span class={classes!("commit__rollout", rollout_state_tone(x.state).class())}>{ rollout_label(&x.id) }</span>
            { a.label.clone() }
        </RouteLink>
        <span class="muted">{ scope }</span>
        <Pill tone={rollout_state_tone(x.state)} label={rollout_state_label(x.state)} title={rollout_note(x).map(AttrValue::from)} />
        if let Some(p) = phase { <span class="muted">{ format!("in {p}") }</span> }
        if x.hosts_total > 0 {
            <span class="rollout-line__progress">
                <Meter segments={rollout_segments(x)} total={x.hosts_total as usize} class="rollout-line__meter" />
                <span class="muted">
                    { format!("{}/{}", x.hosts_done, x.hosts_total) }
                    if x.hosts_failed > 0 { <span class="text-danger">{ format!(" · {} failed", x.hosts_failed) }</span> }
                </span>
            </span>
        }
        if let Some(t) = took { <Took label={t} /> }
        { when(x.started_at, now) }
    </> };
    let aside = html! { <>{ action_key(&a.module, &a.action) }{ review.unwrap_or_default() }</> };
    line(
        format!("rollout-{}", x.id),
        "rollout",
        Some(&x.triggered_by),
        body,
        aside,
    )
}

/// An action run on one target: it opens that target.
fn action_run(r: &ActionRun, now: DateTime<Utc>) -> Html {
    let to = Route::target(&r.target, r.kind);
    let label = r
        .label
        .clone()
        .unwrap_or_else(|| format!("{}/{}", r.module, r.action));
    let took = r
        .finished_at
        .map(|f| short_duration(f.signed_duration_since(r.started_at).num_seconds()));
    let body = html! { <>
        <RouteLink to={to.clone()} classes="commit__subject" title={format!("Open {}", r.target)}>{ label }</RouteLink>
        <span class="muted">{ "on " }<span class="action-line__target">{ r.target.clone() }</span></span>
        <Pill tone={run_tone(r.state)} label={run_label(r.state)} title={r.error.clone().map(AttrValue::from)} />
        if let Some(e) = &r.error { <span class="action-line__error text-danger" title={e.clone()}>{ e.clone() }</span> }
        if let Some(t) = took { <Took label={t} /> }
        { when(r.started_at, now) }
    </> };
    line(
        format!("run-{}", r.id),
        "run",
        r.actor.as_deref(),
        body,
        action_key(&r.module, &r.action),
    )
}
