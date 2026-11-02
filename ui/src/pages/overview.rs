//! Overview (design §12): the fleet's health roll-up with the age of each
//! reading, the latest rollout with its decisions while it is live, the
//! whole fleet laid out on that rollout's stages, the last five revisions
//! with their rollout's outcome, and the agents needing attention.

use std::collections::BTreeMap;

use buckle_api::{Fleet, PhaseState, RolloutDetail, RolloutState, RolloutSummary};
use yew::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Empty, ErrorState, Kind, Loading, PageHead, Panel, Pill, Pipeline, Refresh, RouteLink,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{Bucket, attention, rollup};
use crate::logic::format::{age, commit, count, first_line, relative, rollout_label};
use crate::logic::live::{apply_fleet, apply_revisions, apply_rollout, apply_rollouts};
use crate::logic::pipeline::{fleet_map, layout};
use crate::logic::tone::{
    Tone, revision_status_tone, rollout_finished, rollout_state_label, rollout_state_tone,
};
use crate::pages::rollout::{Ask, controls, use_rollout_actions, wanting_approval};

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
                    actions={latest_id.as_deref().map(|id| html! {
                        <RouteLink to={Route::rollout(id)} classes="btn btn--small">{ "Open rollout" }</RouteLink>
                    }).unwrap_or_default()}>
                    { match (&rollout_live, &rollout.error) {
                        (Some(d), _) => latest(d, auth.is_admin(), &actions.open, now),
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
                    <Panel title="Recent revisions" actions={html! { <RouteLink to={Route::Revisions} classes="btn btn--small btn--subtle">{ "All revisions" }</RouteLink> }}>
                        { match (&revisions_live, &revisions.error) {
                            (Some(list), _) if list.is_empty() => html! { <Empty title="No revisions yet" message="The server has not seen a commit of the configuration repository." /> },
                            (Some(list), _) => recent(list, rollouts_live.as_deref().unwrap_or(&[]), now),
                            (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={revisions.reload.clone()} /> },
                            (None, None) => html! { <Loading /> },
                        } }
                    </Panel>
                </div>
                { rollout_live.as_ref().map(|d| actions.dialog(d, &plan_groups)).unwrap_or_default() }
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

/// The latest rollout's card: what it is, where it stands, and while it is
/// live, its decisions for an administrator.
fn latest(
    d: &RolloutDetail,
    admin: bool,
    open: &Callback<Ask>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let s = &d.summary;
    let live = !rollout_finished(s.state);
    let pipeline = layout(d);
    let wanting = wanting_approval(&pipeline);
    let phase = wanting
        .iter()
        .find(|p| p.state == PhaseState::AwaitingApproval)
        .or(wanting.first());
    html! { <>
        { rollout_head(s, now) }
        if let Some(reason) = &s.pause_reason {
            <p class={classes!("note", (s.state == RolloutState::Paused).then_some("note--warn"))}>
                <strong>{ if s.state == RolloutState::Paused { "Paused: " } else { "Stopped: " } }</strong>{ reason.clone() }
            </p>
        }
        if admin && live {
            <div class="overview__controls" data-testid="rollout-controls">
                if let Some(phase) = phase {
                    <Button kind={Kind::Primary} onclick={{
                        let (open, name) = (open.clone(), phase.name.clone());
                        Callback::from(move |_: MouseEvent| open.emit(Ask::Approve(name.clone())))
                    }}>{ format!("Approve {}…", phase.name) }</Button>
                }
                if s.state == RolloutState::AwaitingApproval {
                    <Button kind={Kind::Danger} onclick={{
                        let open = open.clone();
                        Callback::from(move |_: MouseEvent| open.emit(Ask::Reject))
                    }}>{ "Reject…" }</Button>
                }
                { controls(s, open) }
            </div>
        }
    </> }
}

fn tiles(f: &Fleet, now: chrono::DateTime<chrono::Utc>) -> Html {
    let tiles = rollup(&f.hosts);
    html! {
        <div class="tiles" role="group" aria-label="Fleet health">
            { for tiles.into_iter().map(|t| {
                let tone = match t.bucket {
                    Bucket::Healthy => Tone::Ok,
                    Bucket::Failing => Tone::Danger,
                    Bucket::Held | Bucket::Drifted => Tone::Warn,
                    Bucket::Offline | Bucket::Unknown => Tone::Neutral,
                };
                let ages = match (t.newest, t.oldest) {
                    (Some(n), Some(o)) if age(Some(n), now) != age(Some(o), now) => format!("readings {}–{} old", age(Some(n), now), age(Some(o), now)),
                    (Some(n), _) => format!("reading {} old", age(Some(n), now)),
                    _ => "no readings".into(),
                };
                let ages = if t.bucket == Bucket::Offline && t.count > 0 {
                    ages.replace("readings", "last seen").replace("reading", "last seen")
                } else { ages };
                html! {
                    <RouteLink to={Route::Hosts} query={vec![("show".to_string(), t.bucket.filter().to_string())]}
                        classes={classes!("tile", tone.class())}
                        label={format!("{} {}: {}", t.count, t.bucket.label(), ages)}
                        data_state={t.bucket.filter()}>
                        <span class="tile__label">{ t.bucket.label() }</span>
                        <span class="tile__value">{ t.count }</span>
                        <span class="tile__age">{ if t.count > 0 { ages } else { "—".into() } }</span>
                    </RouteLink>
                }
            }) }
            <div class="tile tile--plain">
                <span class="tile__label">{ "Fleet" }</span>
                <span class="tile__value">{ f.counts.total }</span>
                <span class="tile__age">{ format!("{} online · {} paused · {} pending", f.counts.online, f.counts.paused, f.counts.pending) }</span>
            </div>
        </div>
    }
}

fn rollout_head(s: &RolloutSummary, now: chrono::DateTime<chrono::Utc>) -> Html {
    html! {
        <div class="rollout-head">
            <RouteLink to={Route::rollout(&s.id)} classes="rollout-head__title">
                { format!("Rollout {}", rollout_label(&s.id)) }
            </RouteLink>
            <span class="mono">{ commit(&s.revision) }</span>
            <span class="rollout-head__message">{ format!("“{}”", first_line(&s.revision_message)) }</span>
            <Pill tone={rollout_state_tone(s.state)} label={rollout_state_label(s.state)} />
            if let Some(p) = &s.current_phase { <span class="muted">{ format!("phase {p}") }</span> }
            <span class="muted">{ format!("{}/{} done · started {}", s.hosts_done, s.hosts_total, relative(s.started_at, now)) }</span>
        </div>
    }
}

fn attention_list(
    f: &Fleet,
    rollout: Option<&RolloutDetail>,
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    let list = attention(&f.hosts, f.server_version.as_deref(), rollout, now);
    if list.is_empty() {
        return html! { <Empty title="Nothing needs attention" message="Every agent is registered, connected, current and on a plan its phase approved." /> };
    }
    html! {
        <ul class="attention">
            { for list.into_iter().map(|(host, concerns)| html! {
                <li class="attention__item">
                    <RouteLink to={Route::Host { id: host.clone() }} classes="attention__host mono">{ host.clone() }</RouteLink>
                    <span class="attention__why">{ concerns.iter().map(|c| c.describe(now)).collect::<Vec<_>>().join(" · ") }</span>
                </li>
            }) }
        </ul>
    }
}

fn recent(
    list: &[buckle_api::RevisionSummary],
    rollouts: &[RolloutSummary],
    now: chrono::DateTime<chrono::Utc>,
) -> Html {
    html! {
        <ul class="revlist">
            { for list.iter().take(5).map(|r| {
                let outcome = r.rollout_id.as_ref().and_then(|id| rollouts.iter().find(|x| &x.id == id));
                html! {
                    <li class="revlist__item">
                        <span class="mono revlist__commit">{ commit(&r.commit) }</span>
                        <span class="revlist__message">{ first_line(&r.message) }</span>
                        if r.is_stable { <span class="badge badge--ok">{ "stable" }</span> }
                        { match (r.status, outcome) {
                            (buckle_api::RevisionStatus::Rejected, _) => html! { <Pill tone={revision_status_tone(r.status)} label="rejected" /> },
                            (_, Some(o)) => html! {
                                <RouteLink to={Route::rollout(&o.id)} classes="revlist__rollout">
                                    <Pill tone={rollout_state_tone(o.state)} label={format!("{} {}", rollout_label(&o.id), rollout_state_label(o.state))} />
                                </RouteLink>
                            },
                            _ => html! { <span class="muted">{ "not rolled out" }</span> },
                        } }
                        <span class="muted revlist__age">{ relative(r.created_at, now) }</span>
                    </li>
                }
            }) }
            if list.len() > 5 { <li class="muted">{ format!("and {}", count(list.len() - 5, "older revision", "older revisions")) }</li> }
        </ul>
    }
}
