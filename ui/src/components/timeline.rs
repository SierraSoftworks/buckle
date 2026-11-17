//! The Rollouts timeline ([`crate::logic::timeline`]): each commit as its
//! card, and between them each operator's action shaped like one: title,
//! who ran it, how it went. A diamond on the rail, a bolt and the action
//! colour set it apart; `module/action` stands where a commit has its hash.
//! An action rollout opens its rollout, a run on one target that target.
//! A schedule's rollouts carry an alarm clock and the scheduled colour, with
//! its next occurrence (faded, striped) above everything that happened.

use buckle_api::{ActionRun, RevisionSummary, RolloutState, RolloutSummary, ScheduleInfo};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::avatar::{Avatar, split_author};
use super::changes::Took;
use super::icons;
use super::revision::{RevisionAsk, RolloutProgress, revision_item};
use super::{Pill, RouteLink};
use crate::app::Route;
use crate::logic::format::{count, relative, rollout_label, short_duration, timestamp};
use crate::logic::run::{run_label, run_tone};
use crate::logic::timeline::{Entry, Show, entries};
use crate::logic::tone::rollout_state_label;

#[derive(Properties, PartialEq)]
pub struct TimelineProps {
    pub revisions: Vec<RevisionSummary>,
    pub rollouts: Vec<RolloutSummary>,
    pub runs: Vec<ActionRun>,
    #[prop_or_default]
    pub schedules: Vec<ScheduleInfo>,
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
        &props.schedules,
        props.show,
        props.since,
    );
    html! {
        <ol class="gitlog timeline">
            { for list.iter().map(|e| match e {
                Entry::Commit(r, x) => revision_item(r, *x, stable_at, props.now, &props.open),
                Entry::Rollout(x) => action_rollout(x, props.now),
                Entry::Run(r) => action_run(r, props.now),
                Entry::Upcoming(s, at) => upcoming(s, *at, props.now),
                Entry::Skipped(s, at) => skipped(s, *at, props.now),
            }) }
        </ol>
    }
}

/// An action on the rail, shaped like a commit's card: its title (with a
/// bolt where a commit has its check mark), a meta line, then `progress`;
/// `module/action` at the right, where a commit has its hash. The diamond
/// and the action colour set it apart from the commits.
fn entry(
    key: String,
    kind: &'static str,
    title: Html,
    meta: Html,
    progress: Html,
    aside: Html,
) -> Html {
    let (icon, what) = match kind {
        "scheduled" | "upcoming" | "skipped" => (icons::alarm(), "A scheduled action"),
        _ => (icons::bolt(), "An operator's action"),
    };
    let scheduled = matches!(kind, "scheduled" | "upcoming" | "skipped");
    html! {
        <li {key} class={classes!("gitlog__item", "gitlog__item--action", scheduled.then_some("gitlog__item--scheduled"))} data-action={kind}>
            <span class="gitlog__diamond" aria-hidden="true" />
            <div class={classes!("action-entry", "commit", "commit--link", scheduled.then_some("action-entry--scheduled"))}>
                <div class="commit__main">
                    <div class="commit__line">
                        { title }
                        <span class="action-entry__icon" title={what}>{ icon }</span>
                    </div>
                    <div class="commit__meta muted">{ meta }</div>
                    { progress }
                </div>
                <div class="commit__aside">{ aside }</div>
            </div>
        </li>
    }
}

/// Who ran an action, as a commit names its author.
fn operator(by: Option<&str>) -> Html {
    let Some(by) = by else {
        return html! {};
    };
    let (name, email) = split_author(by);
    let title = email.map(|e| format!("{name} <{e}>"));
    html! { <span class="commit__author" {title}><Avatar author={by.to_string()} />{ name.to_string() }</span> }
}

fn action_key(module: &str, action: &str) -> Html {
    html! { <span class="action-entry__key mono" title="module/action">{ format!("{module}/{action}") }</span> }
}

fn when(at: DateTime<Utc>, now: DateTime<Utc>) -> Html {
    html! { <span title={timestamp(at)}>{ relative(at, now) }</span> }
}

/// An action rolled out across the fleet, as a commit's rollout is drawn:
/// its rollout's name (in the action colour) and the action; who ran it,
/// when and on which targets; then the rollout's progress.
fn action_rollout(x: &RolloutSummary, now: DateTime<Utc>) -> Html {
    let Some(a) = &x.action else {
        return html! {};
    };
    let targets = count(x.hosts_total as usize, "target", "targets");
    let scope = match &a.selector {
        Some(s) => {
            html! { <span>{ format!("{targets} matching ") }<span class="mono">{ s.clone() }</span></span> }
        }
        None => html! { <span>{ targets }</span> },
    };
    let review = (x.state == RolloutState::AwaitingApproval).then(|| {
        html! {
            <yewi::Button size={yewi::Size::Small} variant={yewi::ButtonVariant::Primary} href={yewi::NavTarget::route(Route::rollout(&x.id))}
                attrs={yewi::attrs! { "title" => "This rollout is waiting for an approval" }}>{ "Review…" }</yewi::Button>
        }
    });
    let title = html! {
        <RouteLink to={Route::rollout(&x.id)} classes="commit__subject" title={format!("Open rollout {}", rollout_label(&x.id))}>
            <span class="commit__rollout action-entry__rollout"
                title={format!("Rollout {} {}", rollout_label(&x.id), rollout_state_label(x.state))}>{ rollout_label(&x.id) }</span>
            { a.label.clone() }
        </RouteLink>
    };
    let by = match &a.schedule {
        Some(name) => schedule_name(name, None),
        None => operator(Some(&x.triggered_by)),
    };
    let meta = html! { <>{ by }{ when(x.started_at, now) }{ scope }</> };
    let aside = html! { <>{ action_key(&a.module, &a.action) }{ review.unwrap_or_default() }</> };
    entry(
        format!("rollout-{}", x.id),
        if a.schedule.is_some() {
            "scheduled"
        } else {
            "rollout"
        },
        title,
        meta,
        html! { <RolloutProgress rollout={x.clone()} {now} /> },
        aside,
    )
}

/// An action run on one target: the action, then who ran it where, when,
/// how it went and how long it took. It opens that target.
fn action_run(r: &ActionRun, now: DateTime<Utc>) -> Html {
    let to = Route::target(&r.target, r.kind);
    let label = r
        .label
        .clone()
        .unwrap_or_else(|| format!("{}/{}", r.module, r.action));
    let took = r
        .finished_at
        .map(|f| short_duration(f.signed_duration_since(r.started_at).num_seconds()));
    let title = html! {
        <RouteLink to={to} classes="commit__subject" title={format!("Open {}", r.target)}>{ label }</RouteLink>
    };
    let meta = html! { <>
        { operator(r.actor.as_deref()) }
        { when(r.started_at, now) }
        <span>{ "on " }<span class="action-entry__target">{ r.target.clone() }</span></span>
        <Pill tone={run_tone(r.state)} label={run_label(r.state)} title={r.error.clone().map(AttrValue::from)} />
        if let Some(t) = took { <Took label={t} /> }
        if let Some(e) = &r.error { <span class="action-entry__error text-danger" title={e.clone()}>{ e.clone() }</span> }
    </> };
    entry(
        format!("run-{}", r.id),
        "run",
        title,
        meta,
        html! {},
        action_key(&r.module, &r.action),
    )
}

/// The schedule that runs an action, where an operator's entry names them.
fn schedule_name(name: &str, cron: Option<(&str, &str)>) -> Html {
    let title = match cron {
        Some((c, tz)) => format!("Open schedule {name}: {c} ({tz})"),
        None => format!("Open schedule {name}"),
    };
    html! {
        <RouteLink to={Route::Schedule { name: name.to_owned() }} classes="action-entry__schedule" {title}>
            { name.to_string() }
        </RouteLink>
    }
}

/// The schedule's action, opening the schedule.
fn schedule_title(s: &ScheduleInfo) -> Html {
    html! {
        <RouteLink to={Route::Schedule { name: s.name.clone() }} classes="commit__subject" title={format!("Open schedule {}", s.name)}>
            { schedule_label(s) }
        </RouteLink>
    }
}

/// Which targets a schedule picks.
fn schedule_scope(s: &ScheduleInfo) -> Html {
    let noun = if s.workload { "Placements" } else { "Targets" };
    match &s.selector {
        Some(sel) => {
            html! { <span>{ format!("{noun} matching ") }<span class="mono">{ sel.clone() }</span></span> }
        }
        None => html! { <span>{ "Every target offering it" }</span> },
    }
}

/// When a schedule fires, in words where the server gave them.
pub fn cron_text(s: &ScheduleInfo) -> String {
    if s.cron_text.is_empty() {
        s.cron.clone()
    } else {
        s.cron_text.clone()
    }
}

pub fn schedule_label(s: &ScheduleInfo) -> String {
    s.label
        .clone()
        .unwrap_or_else(|| format!("{}/{}", s.module, s.action))
}

/// A schedule's next occurrence: what it will run, when and where, dashed
/// above what has happened.
fn upcoming(s: &ScheduleInfo, at: DateTime<Utc>, now: DateTime<Utc>) -> Html {
    let title = schedule_title(s);
    let meta = html! { <>
        { schedule_name(&s.name, Some((&s.cron, &s.timezone))) }
        <span title={timestamp(at)}>{ relative(at, now) }</span>
        { schedule_scope(s) }
        <span class="action-entry__cron" title={format!("{} ({})", s.cron, s.timezone)}>{ cron_text(s) }</span>
    </> };
    entry(
        format!("next-{}", s.name),
        "upcoming",
        title,
        meta,
        html! {},
        html! { <>{ action_key(&s.module, &s.action) }<Pill tone={crate::logic::tone::Tone::Neutral} label="Next" /></> },
    )
}

/// A schedule's occurrence that started no rollout, and why.
fn skipped(s: &ScheduleInfo, at: DateTime<Utc>, now: DateTime<Utc>) -> Html {
    let why = s
        .last
        .as_ref()
        .and_then(|l| l.skipped.clone())
        .unwrap_or_default();
    let title = schedule_title(s);
    let meta = html! { <>
        { schedule_name(&s.name, Some((&s.cron, &s.timezone))) }
        { when(at, now) }
        <Pill tone={crate::logic::tone::Tone::Neutral} label="Skipped" title={Some(AttrValue::from(why.clone()))} />
        <span class="action-entry__error" title={why.clone()}>{ why }</span>
    </> };
    entry(
        format!("skipped-{}", s.name),
        "skipped",
        title,
        meta,
        html! {},
        action_key(&s.module, &s.action),
    )
}
