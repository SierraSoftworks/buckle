//! Overview (design §12): the fleet's health roll-up with the age of each
//! reading, the latest rollout with what each module changes and its
//! decisions while it is live, the
//! whole fleet laid out on that rollout's stages, the last five revisions
//! with their rollouts, and the agents needing attention.

use std::collections::BTreeMap;

use buckle_api::{
    Fleet, PhaseState, PlanGroup, RevisionSummary, RolloutDetail, RolloutState, RolloutSummary,
};
use yew::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Changes, Commit, CommitCard, Empty, ErrorState, HostLink, Loading, Meter, PageHead, Panel,
    Pill, Pipeline, Refresh, RevisionLog, RouteLink, Segment, meter::rollout_segments,
    use_revision_actions,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{Bucket, attention, rollup};
use crate::logic::fleet_plan::{Tally, fleet_tally, hosts_changed, summarise};
use crate::logic::format::{age, commit, relative, rollout_label};
use crate::logic::live::{apply_fleet, apply_revisions, apply_rollout, apply_rollouts};
use crate::logic::pipeline::{fleet_map, layout};
use crate::logic::tone::{Tone, rollout_finished, rollout_state_label, rollout_state_tone};
use crate::pages::rollout::{controls, scope_of, use_rollout_actions, wanting_approval};

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
            let hosts: BTreeMap<String, _> =
                f.hosts.iter().map(|h| (h.id.clone(), h.clone())).collect();
            let waiting = latest_id.is_some() && rollout_live.is_none();
            let plan_groups = rollout_live
                .as_ref()
                .map(|d| plans.value().cloned().unwrap_or_else(|| d.plans.clone()))
                .unwrap_or_default();
            html! { <>
                { tiles(f, now) }
                <Panel title="Latest rollout" class="overview__rollout"
                    subtitle={rollout_live.as_ref().map(|d| AttrValue::from(format!("Rollout {} · {} · started {}",
                        rollout_label(&d.summary.id), d.summary.triggered_by, relative(d.summary.started_at, now))))}
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
                        Some(d) => html! { <Pipeline detail={fleet_map(d, &f.hosts)} hosts={hosts.clone()} {now} /> },
                        None if waiting => html! { <Loading /> },
                        None => html! { <Empty title="No stages yet" message="The fleet is laid out on a rollout's stages once the first rollout starts." /> },
                    } }
                </Panel>
                <div class="grid grid--overview">
                    <Panel title="Needs attention" subtitle="Registrations, offline agents, version skew and stale plans">
                        { attention_list(f, rollout_live.as_ref().filter(|d| !rollout_finished(d.summary.state)), now) }
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
                </div>
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
/// the rollout page's approval pane would show, once it has plans to bind.
fn approvable(d: &RolloutDetail, plans: &[PlanGroup]) -> Option<String> {
    if rollout_finished(d.summary.state) {
        return None;
    }
    let pipeline = layout(d);
    let wanting = wanting_approval(&pipeline);
    wanting
        .iter()
        .find(|p| p.state == PhaseState::AwaitingApproval)
        .or(wanting.first())
        .filter(|p| !scope_of(p, plans).digests.is_empty())
        .map(|p| p.name.clone())
}

/// The latest rollout's card: its commit (which opens it), where it stands
/// and what each module changes.
fn latest(
    d: &RolloutDetail,
    revisions: &[RevisionSummary],
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let s = &d.summary;
    html! { <>
        <CommitCard commit={Commit::of_rollout(s, revisions)} rollout={s.clone()} body=true boxed=true {now} />
        { rollout_head(s, d.fleet.as_ref().map(fleet_tally)) }
        { module_changes(d) }
        if let Some(reason) = &s.pause_reason {
            <p class={classes!("note", (s.state == RolloutState::Paused).then_some("note--warn"))}>
                <strong>{ if s.state == RolloutState::Paused { "Paused: " } else { "Stopped: " } }</strong>{ reason.clone() }
            </p>
        }
    </> }
}

fn bucket_tone(b: Bucket) -> Tone {
    match b {
        Bucket::Healthy => Tone::Ok,
        Bucket::Failing => Tone::Danger,
        Bucket::Held | Bucket::Drifted => Tone::Warn,
        Bucket::Offline | Bucket::Unknown => Tone::Neutral,
    }
}

/// The fleet at a glance: its size with how it splits by health, then one
/// tile per health bucket. Drift is not a health state (a drifted host is
/// also healthy, failing, …), so its tile stands apart and says so.
fn tiles(f: &Fleet, now: chrono::DateTime<chrono::Utc>) -> Html {
    let mut tiles = rollup(&f.hosts);
    // Health buckets that add up to the fleet first, drift after them.
    tiles.sort_by_key(|t| t.bucket == Bucket::Drifted);
    let mut segments: Vec<Segment> = tiles
        .iter()
        .filter(|t| t.bucket != Bucket::Drifted)
        .map(|t| {
            let label = match t.bucket {
                Bucket::Unknown => "no reading",
                b => b.filter(),
            };
            Segment::new(t.count, bucket_tone(t.bucket), label)
        })
        .collect();
    segments.push(Segment::new(
        f.counts.pending as usize,
        Tone::Info,
        "awaiting approval",
    ));
    html! {
        <div class="tiles" role="group" aria-label="Fleet health">
            <RouteLink to={Route::Hosts} classes="tile tile--plain tile--fleet" label={format!("{} hosts", f.counts.total)}>
                <span class="tile__label">{ "Fleet" }</span>
                <span class="tile__value">{ f.counts.total }</span>
                <Meter {segments} total={f.counts.total as usize} rest="other" />
                <span class="tile__age">{ format!("{} online · {} paused · {} pending", f.counts.online, f.counts.paused, f.counts.pending) }</span>
            </RouteLink>
            { for tiles.into_iter().map(|t| {
                let tone = bucket_tone(t.bucket);
                let ages = match (t.newest, t.oldest) {
                    (Some(n), Some(o)) if age(Some(n), now) != age(Some(o), now) => format!("readings {}–{} old", age(Some(n), now), age(Some(o), now)),
                    (Some(n), _) => format!("reading {} old", age(Some(n), now)),
                    _ => "no readings".into(),
                };
                let ages = if t.bucket == Bucket::Offline && t.count > 0 {
                    ages.replace("readings", "last seen").replace("reading", "last seen")
                } else { ages };
                let aside = t.bucket == Bucket::Drifted;
                let ages = match (aside, t.count) {
                    (_, 0) => "—".to_string(),
                    (true, _) => format!("also counted above · {ages}"),
                    (false, _) => ages,
                };
                html! {
                    <RouteLink to={Route::Hosts} query={vec![("show".to_string(), t.bucket.filter().to_string())]}
                        classes={classes!("tile", tone.class(), aside.then_some("tile--aside"), (t.count == 0).then_some("tile--zero"))}
                        label={format!("{} {}: {}", t.count, t.bucket.label(), ages)}
                        data_state={t.bucket.filter()}>
                        <span class="tile__label">{ t.bucket.label() }</span>
                        <span class="tile__value">{ t.count }</span>
                        <span class="tile__age">{ ages }</span>
                    </RouteLink>
                }
            }) }
        </div>
    }
}

/// What each module of the latest rollout changes, and on how many of its
/// hosts; modules with nothing to apply are left out.
fn module_changes(d: &RolloutDetail) -> Html {
    let Some(f) = &d.fleet else {
        return html! {};
    };
    let total = f.hosts.len();
    let rows =
        f.modules.iter().filter_map(|m| {
            let summary = summarise(m);
            summary.has_changes().then(|| html! {
            <li class="module-changes__item" data-module={m.module.clone()}>
                <span class="mono module-changes__name">{ m.module.clone() }</span>
                <Changes tally={summary.tally} />
                <span class="muted small">{ hosts_changed(total, summary.changed.len()) }</span>
            </li>
        })
        });
    let removals: Vec<_> = f.removals.iter().filter(|r| r.value.cleanup).collect();
    let removed_on: std::collections::BTreeSet<&String> =
        removals.iter().flat_map(|r| r.hosts.iter()).collect();
    html! {
        <ul class="module-changes" aria-label="Changes by module">
            { for rows }
            if !removals.is_empty() {
                <li class="module-changes__item" data-module="removals">
                    <span class="module-changes__name muted">{ "removals" }</span>
                    <Changes tally={Tally { remove: removals.len(), ..Tally::default() }} />
                    <span class="muted small">{ hosts_changed(total, removed_on.len()) }</span>
                </li>
            }
        </ul>
    }
}

fn rollout_head(s: &RolloutSummary, tally: Option<Tally>) -> Html {
    html! {
        <div class="rollout-head">
            <Pill tone={rollout_state_tone(s.state)} label={rollout_state_label(s.state)} />
            if let Some(tally) = tally { <Changes {tally} /> }
            if let Some(p) = &s.current_phase { <span class="muted">{ format!("in {p}") }</span> }
            <span class="rollout-head__progress">
                <Meter segments={rollout_segments(s)} total={s.hosts_total as usize} />
                <span class="muted small">{ format!("{}/{} done", s.hosts_done, s.hosts_total) }</span>
            </span>
        </div>
    }
}

fn attention_list(
    f: &Fleet,
    rollout: Option<&RolloutDetail>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let list = attention(
        &f.hosts,
        f.desired_agent_version
            .as_deref()
            .or(f.server_version.as_deref()),
        rollout,
        now,
    );
    if list.is_empty() {
        return html! { <Empty title="Nothing needs attention" message="Every agent is registered, connected, current and on a plan its phase approved." /> };
    }
    html! {
        <ul class="attention">
            { for list.into_iter().map(|(host, concerns)| {
                let summary = f.hosts.iter().find(|h| h.id == host).cloned();
                let worst = [Tone::Danger, Tone::Warn]
                    .into_iter()
                    .find(|t| concerns.iter().any(|c| c.tone() == *t))
                    .unwrap_or(Tone::Info);
                html! {
                    <li class={classes!("attention__item", worst.class())} data-host={host.clone()}>
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
}
