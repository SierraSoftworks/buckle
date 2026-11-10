//! Overview (design §12): one bar of every host and workload by its state
//! now, the latest rollout's card with its decisions while it is live, the
//! whole fleet laid out on that rollout's stages, and the last five
//! revisions with their rollouts. What needs attention is the app bar's
//! bell.

use std::collections::BTreeMap;

use buckle_api::{Fleet, PlanGroup, RevisionSummary, RolloutDetail, RolloutState};
use yew::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::revision::RolloutProgress;
use crate::components::{
    Commit, CommitCard, Empty, ErrorState, Loading, Meter, PageHead, Panel, Pipeline, Refresh,
    RevisionLog, RouteLink, Segment, use_revision_actions,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{FleetStates, fleet_states};
use crate::logic::format::{commit, count};
use crate::logic::live::{apply_fleet, apply_revisions, apply_rollout, apply_rollouts};
use crate::logic::pipeline::{fleet_map, layout};
use crate::logic::tone::{Tone, rollout_finished};
use crate::pages::rollout::{
    controls, next_approval, phase_approvable, use_rollout_actions, wanting_approval,
};

#[function_component(Overview)]
pub fn overview() -> Html {
    let now = use_now(15);
    let auth = use_auth();
    let fleet = use_fetch((), |_| api::fleet());
    let fleet_live = use_live_value(&fleet, apply_fleet);
    let rollouts = use_fetch((), |_| async { api::rollouts(0).await.map(|p| p.items) });
    let rollouts_live = use_live_value(&rollouts, apply_rollouts);
    let active_id = fleet_live.as_ref().and_then(|f| f.active_rollout.clone());
    // The rollout in progress, or else the newest: its stages lay the fleet out.
    let latest_id = active_id.clone().or_else(|| {
        rollouts_live
            .as_ref()
            .and_then(|list| list.first().map(|r| r.id.clone()))
    });
    let rollout = use_fetch(latest_id.clone(), |id| async move {
        match id {
            Some(id) => api::rollout(&id).await.map(Some),
            None => Ok(None),
        }
    });
    let rollout_live = use_live_value(&rollout, |d: &mut Option<RolloutDetail>, e| {
        if let Some(d) = d {
            apply_rollout(d, e)
        }
    })
    .flatten()
    // A stale answer for the previous latest rollout is not shown.
    .filter(|d| Some(&d.summary.id) == latest_id.as_ref());
    // The plans an approval from here binds, while the rollout is live.
    let plans = use_fetch(active_id.clone(), |id| async move {
        match id {
            Some(id) => api::rollout_plans(&id).await,
            None => Ok(Vec::new()),
        }
    });
    let actions = use_rollout_actions(
        latest_id.clone().unwrap_or_default(),
        rollout.set.reform(Some),
    );
    let revisions = use_fetch((), |_| async { api::revisions(0).await.map(|p| p.items) });
    let revisions_live = use_live_value(&revisions, apply_revisions);
    let revision_actions = use_revision_actions();

    let content = match (&fleet_live, &fleet.error) {
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={fleet.reload.clone()} /> },
        (None, None) => html! { <Loading label="Reading the fleet…" /> },
        (Some(f), _) => {
            let hosts: BTreeMap<String, _> = f
                .hosts
                .iter()
                .chain(&f.clusters)
                .map(|h| (h.id.clone(), h.clone()))
                .collect();
            let waiting = latest_id.is_some() && rollout_live.is_none();
            let plan_groups = rollout_live
                .as_ref()
                .map(|d| plans.value().cloned().unwrap_or_else(|| d.plans.clone()))
                .unwrap_or_default();
            html! { <>
                { fleet_bar(f) }
                <Panel title="Latest rollout" class="overview__rollout"
                    actions={rollout_live.as_ref().filter(|_| auth.is_admin())
                        .map(|d| controls(d, approvable(d, &plan_groups).as_deref(), &actions.open))
                        .unwrap_or_default()}>
                    { match (&rollout_live, &rollout.error) {
                        (Some(d), _) => latest(d, revisions_live.as_deref().unwrap_or(&[]), now),
                        (None, Some(e)) if waiting => html! { <ErrorState error={e.clone()} retry={rollout.reload.clone()} /> },
                        (None, _) if waiting => html! { <Loading /> },
                        (None, _) => html! { <Empty title="No rollouts yet"
                            message={f.stable_revision.as_ref().map(|r| format!("Every host converges to the stable revision {}.", commit(r)))} /> },
                    } }
                </Panel>
                <Panel title="Fleet" class="overview__fleet"
                    subtitle="Every host on the latest rollout's stages: its state in that rollout, with its current health and connection">
                    { match &rollout_live {
                        Some(d) => html! { <Pipeline detail={fleet_map(d, &f.hosts, &f.clusters)} hosts={hosts.clone()} {now} /> },
                        None if waiting => html! { <Loading /> },
                        None => html! { <Empty title="No stages yet" message="The fleet is laid out on a rollout's stages once the first rollout starts." /> },
                    } }
                </Panel>
                <Panel title="Rollouts" actions={html! { <RouteLink to={Route::Rollouts} classes="btn btn--small btn--subtle">{ "All rollouts" }</RouteLink> }}>
                    { match (&revisions_live, &revisions.error) {
                        (Some(list), _) if list.is_empty() => html! { <Empty title="No revisions yet" message="The server has not seen a commit of the configuration repository." /> },
                        (Some(list), _) => html! {
                            <RevisionLog revisions={list.clone()} rollouts={rollouts_live.clone().unwrap_or_default()} {now}
                                open={revision_actions.open.clone()} limit={Some(5)} />
                        },
                        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={revisions.reload.clone()} /> },
                        (None, None) => html! { <Loading /> },
                    } }
                </Panel>
                { rollout_live.as_ref().map(|d| actions.dialog(d, &plan_groups)).unwrap_or_default() }
                { revision_actions.dialog(rollouts_live.as_deref().unwrap_or(&[])) }
            </> }
        }
    };

    html! {
        <div class="page page--wide">
            <PageHead title="Overview" subtitle={html! { "The fleet from each host's latest health report and agent liveness." }}
                actions={html! { <Refresh loading={fleet.loading || rollout.loading} reload={{
                    let (a, b, c, d, e) = (fleet.reload.clone(), rollout.reload.clone(), revisions.reload.clone(), rollouts.reload.clone(), plans.reload.clone());
                    Callback::from(move |_| { a.emit(()); b.emit(()); c.emit(()); d.emit(()); e.emit(()); })
                }} /> }} />
            { content }
        </div>
    }
}

/// The phase an approval from the latest rollout's controls is for: the one
/// the rollout page's plans panel would approve, once the phases before it
/// have finished and it has plans to bind.
fn approvable(d: &RolloutDetail, plans: &[PlanGroup]) -> Option<String> {
    if rollout_finished(d.summary.state) {
        return None;
    }
    let pipeline = layout(d);
    next_approval(&wanting_approval(&pipeline))
        .filter(|p| phase_approvable(d, p, plans))
        .map(|p| p.name.clone())
}

/// The latest rollout's card: its commit (which opens it) with how the
/// rollout is going, the same card as on Rollouts.
fn latest(
    d: &RolloutDetail,
    revisions: &[RevisionSummary],
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let mut s = d.summary.clone();
    s.changes = s.changes.or_else(|| {
        d.fleet
            .as_ref()
            .filter(|f| !f.hosts.is_empty())
            .map(|f| f.changes())
    });
    html! { <>
        <CommitCard commit={Commit::of_rollout(&s, revisions)} rollout={s.clone()} body=true boxed=true {now}>
            <RolloutProgress rollout={s.clone()} {now} />
        </CommitCard>
        if let Some(reason) = &s.pause_reason {
            <p class={classes!("note", (s.state == RolloutState::Paused).then_some("note--warn"))}>
                <strong>{ if s.state == RolloutState::Paused { "Paused: " } else { "Stopped: " } }</strong>{ reason.clone() }
            </p>
        }
    </> }
}

/// The fleet at a glance: one bar of every host and workload by its state
/// now, with the unusual states named beside the counts. Each part links to
/// the hosts (or clusters) in that state.
fn fleet_bar(f: &Fleet) -> Html {
    let s = fleet_states(&f.hosts, &f.clusters);
    let parts = parts(&s);
    let segments: Vec<Segment> = parts
        .iter()
        .map(|(n, tone, label, _)| Segment::new(*n, *tone, label))
        .collect();
    // Deploying is workloads only: those are on the Workloads page.
    let to = |show: &str| {
        if show.is_empty() {
            Route::Workloads
        } else {
            Route::Hosts
        }
    };
    let link = |show: &str| match show {
        "" => Vec::new(),
        s => vec![("show".to_string(), s.to_string())],
    };
    html! {
        <section class="fleet-bar" aria-label="Fleet health" data-testid="fleet-bar">
            <div class="fleet-bar__head">
                <RouteLink to={Route::Hosts} classes="fleet-bar__total">{ count(s.hosts, "host", "hosts") }</RouteLink>
                if s.workloads > 0 {
                    <RouteLink to={Route::Workloads} classes="fleet-bar__total">{ count(s.workloads, "workload", "workloads") }</RouteLink>
                }
                <span class="fleet-bar__legend">
                    { for parts.iter().filter(|(n, ..)| *n > 0).map(|(n, tone, label, show)| html! {
                        <RouteLink to={to(show)} query={link(show)}
                            classes={classes!("fleet-bar__part", tone.class())} data_state={*label}>
                            { format!("{n} {label}") }
                        </RouteLink>
                    }) }
                    if s.drifted > 0 {
                        <RouteLink to={Route::Hosts} query={link("drifted")}
                            classes={classes!("fleet-bar__part", "fleet-bar__part--aside", Tone::Warn.class())} data_state="drifted"
                            title="Drift is not a state of its own: these hosts are also counted above">
                            { format!("{} drifted", s.drifted) }
                        </RouteLink>
                    }
                </span>
            </div>
            <Meter {segments} total={s.total()} rest="other" class="fleet-bar__meter" />
        </section>
    }
}

/// The bar's parts in order: each count, its tone, its name and the hosts
/// filter it links to.
fn parts(s: &FleetStates) -> [(usize, Tone, &'static str, &'static str); 7] {
    [
        (s.healthy, Tone::Ok, "healthy", "healthy"),
        (s.deploying, Tone::Active, "deploying", ""),
        (s.held, Tone::Warn, "held", "held"),
        (s.failing, Tone::Danger, "failing", "failing"),
        (s.offline, Tone::Neutral, "offline", "offline"),
        (s.unknown, Tone::Neutral, "no reading", "unknown"),
        (s.pending, Tone::Info, "awaiting approval", "pending"),
    ]
}
