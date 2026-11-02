//! Overview (design §12): the fleet's health roll-up with the age of each
//! reading, the active rollout as a pipeline, the last five revisions with
//! their rollout's outcome, and the agents needing attention.

use std::collections::BTreeMap;

use buckle_api::{Fleet, RolloutDetail, RolloutSummary};
use yew::prelude::*;

use crate::api;
use crate::app::Route;
use crate::components::{
    Empty, ErrorState, Loading, PageHead, Panel, Pill, Pipeline, Refresh, RouteLink,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::fleet::{Bucket, attention, rollup};
use crate::logic::format::{age, commit, count, first_line, relative};
use crate::logic::live::{apply_fleet, apply_revisions, apply_rollout, apply_rollouts};
use crate::logic::tone::{Tone, revision_status_tone, rollout_state_label, rollout_state_tone};

#[function_component(Overview)]
pub fn overview() -> Html {
    let now = use_now(15);
    let fleet = use_fetch((), |_| api::fleet());
    let fleet_live = use_live_value(&fleet, apply_fleet);
    let active_id = fleet_live.as_ref().and_then(|f| f.active_rollout.clone());
    let rollout = use_fetch(active_id.clone(), |id| async move {
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
    .flatten();
    let revisions = use_fetch((), |_| async { api::revisions(0).await.map(|p| p.items) });
    let revisions_live = use_live_value(&revisions, apply_revisions);
    let rollouts = use_fetch((), |_| async { api::rollouts(0).await.map(|p| p.items) });
    let rollouts_live = use_live_value(&rollouts, apply_rollouts);

    let content = match (&fleet_live, &fleet.error) {
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={fleet.reload.clone()} /> },
        (None, None) => html! { <Loading label="Reading the fleet…" /> },
        (Some(f), _) => {
            let hosts: BTreeMap<String, _> =
                f.hosts.iter().map(|h| (h.id.clone(), h.clone())).collect();
            html! { <>
                { tiles(f, now) }
                <div class="grid grid--overview">
                    <Panel title="Active rollout" class="grid__wide"
                        actions={active_id.clone().map(|id| html! {
                            <RouteLink to={Route::Rollout { id }} classes="btn btn--small">{ "Open rollout" }</RouteLink>
                        }).unwrap_or_default()}>
                        { match (&active_id, &rollout_live, &rollout.error) {
                            (None, _, _) => html! { <Empty title="No rollout in progress"
                                message={f.stable_revision.as_ref().map(|r| format!("Every host converges to the stable revision {}.", commit(r)))} /> },
                            (Some(_), Some(d), _) => html! { <>
                                { rollout_head(&d.summary, now) }
                                <Pipeline detail={d.clone()} hosts={hosts.clone()} compact=true {now} />
                            </> },
                            (Some(_), None, Some(e)) => html! { <ErrorState error={e.clone()} retry={rollout.reload.clone()} /> },
                            (Some(_), None, None) => html! { <Loading /> },
                        } }
                    </Panel>
                    <Panel title="Needs attention" subtitle="Registrations, offline agents, version skew and stale plans">
                        { attention_list(f, rollout_live.as_ref(), now) }
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
            </> }
        }
    };

    html! {
        <div class="page">
            <PageHead title="Overview" subtitle={html! { "The fleet from each host's latest health report and agent liveness." }}
                actions={html! { <Refresh loading={fleet.loading || rollout.loading} reload={{
                    let (a, b, c) = (fleet.reload.clone(), rollout.reload.clone(), revisions.reload.clone());
                    Callback::from(move |_| { a.emit(()); b.emit(()); c.emit(()); })
                }} /> }} />
            { content }
        </div>
    }
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
            <RouteLink to={Route::Rollout { id: s.id.clone() }} classes="rollout-head__title">
                { format!("Rollout #{}", s.id) }
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
                                <RouteLink to={Route::Rollout { id: o.id.clone() }} classes="revlist__rollout">
                                    <Pill tone={rollout_state_tone(o.state)} label={format!("#{} {}", o.id, rollout_state_label(o.state))} />
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
