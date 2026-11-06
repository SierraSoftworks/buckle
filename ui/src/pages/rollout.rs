//! Rollout detail (design §10–§12): the pipeline, the approval pane with
//! deduplicated plans and their diffs, each host's lifecycle ticker and run
//! report (live while it applies, with the module and step in progress on
//! its block), and the operator decisions, offered to admins only. Every
//! timeline counts from the rollout's start (`T+2m`).

use std::collections::BTreeMap;

use buckle_api::{
    ApproveRequest, ContinueRequest, HostRolloutState, HostSummary, LiveEvent, PlanGroup,
    RolloutDetail, RolloutEvent, RolloutHost, RolloutPhase, RolloutState,
};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use crate::components::ui::{Note, NoteKind};

use crate::api::{self, ApiError};
use crate::app::{Route, use_auth};
use crate::components::icons;
use crate::components::meter::stage_segments;
use crate::components::ui::Copyable;
use crate::components::{
    Button, Changes, Commit, CommitCard, Confirm, Countdown, ErrorBlock, ErrorState, HostLink,
    HoverTip, Kind, Loading, MenuItem, Meter, PageHead, Panel, Pill, Pipeline, PlanViews, Refresh,
    RouteLink, RunReportView, Segmented, SplitButton, TraceLink, use_toast,
};
use crate::hooks::{RunSource, use_debounced, use_fetch, use_live_value, use_now, use_run_report};
use crate::logic::controls::{Decision, Standing, decisions, replan_label};
use crate::logic::fleet_plan::{Phases, fleet_tally};
use crate::logic::format::{count, relative, rollout_label, short, since, timestamp};
use crate::logic::live::{apply_fleet, apply_rollout};
use crate::logic::pipeline::{self, GateKind, GateState, host_checks, layout};
use crate::logic::plan::{ApprovalScope, approval_scope, attempt_options, host_tally};
use crate::logic::run::RolloutActivity;
use crate::logic::ticker::{Source, Ticker, ticker};
use crate::logic::tone::{
    Tone, awaited_reboot, host_error_headline, host_error_tone, host_state_label, host_state_tone,
    phase_state_label, phase_state_tone, rollout_finished, rollout_state_label, rollout_state_tone,
};
use crate::logic::units;
use crate::store::use_live;
use serde::{Deserialize, Serialize};
use yew_router::prelude::{use_location, use_navigator};

#[derive(Properties, PartialEq)]
pub struct Props {
    pub id: String,
}

/// The most reports held at once for the blocks' "in progress" detail: a
/// phase applies a few hosts at a time, and the updates cover the rest.
const MAX_ACTIVITY_REPORTS: usize = 24;

/// The decision an open dialog is asking about.
#[derive(Clone, PartialEq)]
pub(crate) enum Ask {
    Approve(String),
    /// Confirmed: the phase and exactly the digests the dialog showed.
    Bind(String, Vec<String>),
    Reject,
    /// A rollout action (the API's), or `skip-phase`: resuming past failures.
    Action(&'static str),
    Host(&'static str, String),
    /// A phase's action (`expedite`) and the phase.
    Phase(&'static str, String),
    Continue(String, Vec<String>),
}

/// The Targets card's view, kept in the URL so a link shows what its
/// sender saw (`?targets=all`).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct TargetsQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    targets: Option<String>,
}

#[function_component(RolloutPage)]
pub fn rollout_page(props: &Props) -> Html {
    let now = use_now(5);
    let auth = use_auth();
    let location = use_location();
    let all_targets = location
        .and_then(|l| l.query::<TargetsQuery>().ok())
        .is_some_and(|q| q.targets.as_deref() == Some("all"));
    let on_targets = {
        let (navigator, id) = (use_navigator(), props.id.clone());
        Callback::from(move |v: AttrValue| {
            if let Some(n) = &navigator {
                let route = Route::Rollout { id: id.clone() };
                let _ = if v == "all" {
                    n.replace_with_query(
                        &route,
                        &TargetsQuery {
                            targets: Some("all".into()),
                        },
                    )
                } else {
                    n.replace_with_query(&route, &TargetsQuery::default())
                };
            }
        })
    };
    let detail = use_fetch(
        props.id.clone(),
        |id| async move { api::rollout(&id).await },
    );
    // The rollout's id, which its live updates are matched on, once known:
    // the URL may name it instead (`/rollouts/word.word.word`).
    let id = detail
        .data
        .as_ref()
        .map(|d| d.1.summary.id.clone())
        .unwrap_or_else(|| props.id.clone());
    // The URL settles on the rollout's name (or its id, without one).
    {
        let navigator = use_navigator();
        let canonical = detail
            .data
            .as_ref()
            .map(|d| Route::rollout(&d.1.summary.id))
            .filter(|r| {
                *r != Route::Rollout {
                    id: props.id.clone(),
                }
            });
        use_effect_with(canonical, move |route| {
            if let (Some(route), Some(n)) = (route, navigator) {
                n.replace(route);
            }
            || ()
        });
    }
    // Host entries, the summary and timeline entries arrive on the stream
    // and are applied as they come. Phase states and approvals do not: when
    // the rollout itself changes, its detail is fetched again once the burst
    // of events has settled.
    let live = use_live_value(&detail, apply_rollout);
    let store = use_live();
    let touched = detail
        .data
        .as_ref()
        .and_then(|d| store.log.since(d.0))
        .map(|events| {
            events
                .iter()
                .filter(
                    |e| matches!(&***e, LiveEvent::RolloutChanged { rollout } if rollout.id == id),
                )
                .count()
        })
        .unwrap_or(0);
    let settled = use_debounced(touched, 1_500);
    // The plans are fetched again with the detail: a host that plans (or
    // re-plans) after the page opened is otherwise missing from the
    // approval pane's scope, leaving nothing to approve.
    let plans = use_fetch(props.id.clone(), |id| async move {
        api::rollout_plans(&id).await
    });
    {
        let (reload, reload_plans) = (detail.reload.clone(), plans.reload.clone());
        use_effect_with(settled, move |n| {
            if *n > 0 {
                reload.emit(());
                reload_plans.emit(());
            }
            || ()
        });
    }
    let fleet = use_fetch((), |_| api::fleet());
    let fleet = use_live_value(&fleet, apply_fleet);
    // The rollout's commit as the revisions list has it (author, age).
    let revisions = use_fetch((), |_| async { api::revisions(0).await.map(|p| p.items) });
    // What the applying hosts are doing, for their blocks: each one's
    // report fetched once (all at the same time) and kept current by the
    // run updates, or the updates alone while there is none.
    let applying: Vec<String> = live
        .as_ref()
        .map(|d| {
            let mut v: Vec<String> = d
                .phases
                .iter()
                .flat_map(|p| &p.hosts)
                .filter(|h| h.state == HostRolloutState::Applying)
                .map(units::key)
                .collect();
            v.sort();
            v
        })
        .unwrap_or_default();
    let tracker = use_mut_ref(RolloutActivity::default);
    let redraw = use_force_update();
    let (activity, to_ask) = {
        let mut t = tracker.borrow_mut();
        t.sync(&id, store.epoch, &applying, &store.log);
        (t.describe(), t.to_ask(&applying, MAX_ACTIVITY_REPORTS))
    };
    {
        let (tracker, id) = (tracker.clone(), id.clone());
        let (epoch, seq) = (store.epoch, store.log.seq);
        use_effect_with(to_ask, move |hosts| {
            if !hosts.is_empty() {
                tracker.borrow_mut().asking(hosts);
                let hosts = hosts.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let reports = futures::future::join_all(
                        hosts.iter().map(|h| api::rollout_host_report(&id, h)),
                    )
                    .await;
                    tracker.borrow_mut().answered(
                        epoch,
                        seq,
                        hosts.into_iter().zip(reports.into_iter().map(Result::ok)),
                    );
                    redraw.force_update();
                });
            }
            || ()
        });
    }
    let actions = use_rollout_actions(id.clone(), detail.set.clone());
    // The Plans card: Combined (the union) or By target (distinct plans).
    let plans_view = use_state(|| AttrValue::from("combined"));

    let Some(d) = live else {
        return html! {
            <div class="page">
                <PageHead title={format!("Rollout {}", rollout_label(&props.id))} />
                { match &detail.error {
                    Some(e) => html! { <ErrorState error={e.clone()} retry={detail.reload.clone()} /> },
                    None => html! { <Loading label="Loading the rollout…" /> },
                } }
            </div>
        };
    };

    // The plans endpoint is the authority for diffs; the detail's copy stands
    // in until it has answered.
    let plan_groups: Vec<PlanGroup> = plans.value().cloned().unwrap_or_else(|| d.plans.clone());
    let hosts: BTreeMap<String, _> = fleet
        .map(|f| f.hosts.into_iter().map(|h| (h.id.clone(), h)).collect())
        .unwrap_or_default();
    let open = actions.open.clone();

    let s = &d.summary;
    let admin = auth.is_admin();
    let finished = rollout_finished(s.state);

    let subtitle = html! { <>
        if let Some(a) = &s.action {
            <p class="rollout-action" data-testid="rollout-action">
                <Pill tone={Tone::Info} label="action" />{ " " }
                <span class="strong">{ a.label.clone() }</span>{ " " }
                <span class="badge mono">{ format!("{}/{}", a.module, a.action) }</span>
                <span class="muted">{ match &a.selector {
                    Some(sel) => format!(" on the targets {sel} matches, through the phases of {}", short(&s.revision)),
                    None => format!(" on every target offering it, through the phases of {}", short(&s.revision)),
                } }</span>
            </p>
        } else {
            <CommitCard commit={Commit::of_rollout(s, revisions.value().map(|v| v.as_slice()).unwrap_or(&[]))}
                link=false body=true boxed=true {now} />
        }
        <div class="rollout-meta">
            <Pill tone={rollout_state_tone(s.state)} label={rollout_state_label(s.state)} />
            if let Some(f) = &d.fleet { <Changes tally={fleet_tally(f)} /> }
            <span class="muted">{ format!("{} · started {}", s.triggered_by, relative(s.started_at, now)) }</span>
            if let Some(f) = s.finished_at { <span class="muted">{ format!("finished {}", relative(f, now)) }</span> }
            <TraceLink id={s.trace_id.clone()} url={s.trace_url.clone()} />
            if s.name.is_some() { <Copyable text={s.id.clone()} label={format!("{}…", short(&s.id))} /> }
        </div>
    </> };

    let pipeline = layout(&d);
    let wanting = wanting_approval(&pipeline);
    // The head's approval is for the next phase wanting one, once the
    // phases before it have finished.
    let focused = next_approval(&wanting).filter(|p| phase_approvable(&d, p, &plan_groups));

    let head_actions = html! { <>
        <Refresh loading={detail.loading} reload={{ let (a, b) = (detail.reload.clone(), plans.reload.clone()); Callback::from(move |_| { a.emit(()); b.emit(()); }) }} />
        if admin { { controls(&d, focused.map(|p| p.name.as_str()), &open) } }
    </> };

    let approving: &[&RolloutPhase] = if finished { &[] } else { &wanting };
    let plans_panel = plans_panel(approving, &d, &plan_groups, admin, &open, &plans_view);

    let dialog = actions.dialog(&d, &plan_groups);

    html! {
        <div class="page page--wide">
            <PageHead title={format!("Rollout {}", rollout_label(&s.id))} {subtitle} actions={head_actions} />
            if let Some(reason) = &s.pause_reason {
                <div class={classes!("banner", if s.state == RolloutState::Paused { "banner--warn" } else { "banner--info" })} role="status">
                    <strong>{ if s.state == RolloutState::Paused { "Paused: " } else { "Stopped: " } }</strong>{ reason.clone() }
                    if admin && s.state == RolloutState::Paused {
                        <span class="muted">{ if failed(&d).is_empty() {
                            " Resume, roll back or abort the rollout."
                        } else {
                            " Retry what failed once its cause is fixed, act on each from its row's menu, or skip this phase, roll back or abort the rollout."
                        } }</span>
                    }
                </div>
            }
            if let Some(by) = s.superseded_by.as_ref().filter(|_| s.state == RolloutState::Skipped) {
                <div class="banner banner--info" role="status">
                    <strong>{ "Skipped: " }</strong>{ "rollout " }<RouteLink to={Route::rollout(by)}>{ rollout_label(by) }</RouteLink>
                    { " of a newer revision superseded this one while it was queued, so it never started." }
                </div>
            }
            <Panel title="Pipeline" subtitle={{
                let all = || d.phases.iter().flat_map(|p| &p.hosts);
                let workloads = all().filter(|h| units::is_placement(h)).count();
                let clusters = all().filter(|h| h.kind == buckle_api::TargetKind::Cluster && h.workload.is_none()).count();
                let hosts = all().count() - workloads - clusters;
                let mut parts = vec![count(hosts, "host", "hosts")];
                if workloads > 0 { parts.push(count(workloads, "workload", "workloads")); }
                if clusters > 0 { parts.push(count(clusters, "cluster", "clusters")); }
                let unchanged = all().filter(|h| units::unchanged(h)).count();
                let mut line = format!("{} in {} stages · {}/{} done", parts.join(", "), d.phases.len(), s.hosts_done, s.hosts_total);
                if unchanged > 0 { line.push_str(&format!(" · {unchanged} unchanged")); }
                line
            }}>
                <Pipeline detail={d.clone()} hosts={hosts.clone()} activity={activity.clone()} {now} />
            </Panel>
            { plans_panel }
            <Panel title="Targets" subtitle={{
                let all = || d.phases.iter().flat_map(|p| &p.hosts);
                let unchanged = all().filter(|h| units::unchanged(h)).count();
                format!("{} changed · {unchanged} unchanged · by stage; click a row for its way through this rollout", all().count() - unchanged)
            }} actions={html! {
                <Segmented label="Targets shown" testid="targets-view"
                    value={if all_targets { "all" } else { "changed" }}
                    options={vec![("changed".into(), "Changed".into()), ("all".into(), "All".into())]}
                    on_change={on_targets} />
            }}>
                <HostTable detail={d.clone()} plans={plan_groups.clone()} admin={admin && !finished} on_ask={open.clone()} on_run={actions.now.clone()} {activity} {hosts} all={all_targets} {now} />
            </Panel>
            <Panel title="Timeline" subtitle="Rollout and phase transitions">
                { timeline(d.events.iter().filter(|e| e.host.is_none()).collect(), s.started_at) }
            </Panel>
            { dialog }
        </div>
    }
}

/// The decisions an operator can take on a rollout, and the dialog that
/// confirms the one asked about: shared by this page and the Overview's
/// latest rollout card.
pub(crate) struct RolloutActions {
    ask: Option<Ask>,
    busy: bool,
    error: Option<String>,
    pub open: Callback<Ask>,
    /// Runs a decision at once, without a dialog (a re-plan from an error).
    pub now: Callback<Ask>,
    close: Callback<()>,
    run: Callback<(Ask, String)>,
}

impl RolloutActions {
    /// The open dialog, if one is.
    pub fn dialog(&self, d: &RolloutDetail, plans: &[PlanGroup]) -> Html {
        self.ask
            .as_ref()
            .map(|a| {
                dialog(
                    a,
                    d,
                    plans,
                    self.busy,
                    self.error.clone(),
                    &self.run,
                    &self.close,
                )
            })
            .unwrap_or_default()
    }
}

/// Runs a confirmed decision on rollout `id`, hands the rollout it answers
/// with to `set`, and says how it went.
#[hook]
pub(crate) fn use_rollout_actions(id: String, set: Callback<RolloutDetail>) -> RolloutActions {
    let toast = use_toast();
    let ask = use_state(|| None::<Ask>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let open = {
        let (ask, error) = (ask.clone(), error.clone());
        Callback::from(move |a: Ask| {
            error.set(None);
            ask.set(Some(a));
        })
    };
    let close = {
        let ask = ask.clone();
        Callback::from(move |_| ask.set(None))
    };
    // `in_dialog`: the decision was confirmed in a dialog, which shows how
    // it failed; otherwise a toast does.
    let send = {
        let (busy, error, ask, set, toast) = (
            busy.clone(),
            error.clone(),
            ask.clone(),
            set.clone(),
            toast.clone(),
        );
        let id = id.clone();
        Callback::from(move |(what, note, in_dialog): (Ask, String, bool)| {
            let (busy, error, ask, set, toast, id) = (
                busy.clone(),
                error.clone(),
                ask.clone(),
                set.clone(),
                toast.clone(),
                id.clone(),
            );
            let label = rollout_label(&id);
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                let (result, done): (Result<RolloutDetail, ApiError>, String) = match &what {
                    Ask::Approve(_) => return,
                    Ask::Bind(phase, digests) => (
                        api::approve(
                            &id,
                            &ApproveRequest {
                                phase: phase.clone(),
                                plan_digests: digests.clone(),
                                note,
                            },
                        )
                        .await,
                        format!(
                            "Approved {phase}: {} bound.",
                            count(digests.len(), "plan", "plans")
                        ),
                    ),
                    Ask::Reject => (
                        api::reject(&id, &note).await,
                        format!("Rollout {label} rejected."),
                    ),
                    Ask::Action(action) => (
                        api::rollout_action(
                            &id,
                            if *action == "skip-phase" {
                                "resume"
                            } else {
                                action
                            },
                            &note,
                        )
                        .await,
                        format!("Rollout {label}: {action} sent."),
                    ),
                    Ask::Host(action, host) => (
                        // Redeploying a host that is done force applies it again.
                        api::rollout_host_action(
                            &id,
                            host,
                            if *action == "redeploy" {
                                "force"
                            } else {
                                action
                            },
                            &note,
                        )
                        .await,
                        format!("{host}: {action} sent."),
                    ),
                    Ask::Phase(action, phase) => (
                        api::rollout_phase_action(&id, phase, action, &note).await,
                        format!("{phase}: {action} sent."),
                    ),
                    Ask::Continue(host, checks) => (
                        api::continue_host(
                            &id,
                            host,
                            &ContinueRequest {
                                waive_checks: checks.clone(),
                                note,
                            },
                        )
                        .await,
                        format!("{host}: continuing with {} waived.", checks.join(", ")),
                    ),
                };
                busy.set(false);
                match result {
                    Ok(detail) => {
                        set.emit(detail);
                        ask.set(None);
                        toast.ok(done);
                    }
                    Err(e) => {
                        let message = e.to_string();
                        if e.is_forbidden() {
                            toast.error(format!("Not permitted: {message}"));
                        } else if !in_dialog {
                            toast.error(message.clone());
                        }
                        error.set(Some(message));
                    }
                }
            });
        })
    };

    let run = send.reform(|(a, note): (Ask, String)| (a, note, true));
    let now = send.reform(|a: Ask| (a, String::new(), false));
    RolloutActions {
        ask: (*ask).clone(),
        busy: *busy,
        error: (*error).clone(),
        open,
        now,
        close,
        run,
    }
}

/// The rollout-wide decisions for its state as one split button: approve
/// the phase waiting for it (`approve`, once it has plans to bind), else
/// pause, retry or resume, with the rest behind its menu. Nothing once it
/// has finished.
pub(crate) fn controls(
    detail: &RolloutDetail,
    approve: Option<&str>,
    open: &Callback<Ask>,
) -> Html {
    let standing = Standing {
        failed: failed(detail).len(),
        baking: baking(detail).len(),
        action: detail.summary.action.is_some(),
    };
    let Some(d) = decisions(detail.summary.state, approve, standing) else {
        return html! {};
    };
    let ask = |d: &Decision| {
        let (o, a) = (open.clone(), ask_for(d));
        Callback::from(move |_: MouseEvent| o.emit(a.clone()))
    };
    let item = |d: &Decision| {
        let kind = match d {
            Decision::Approve(_) | Decision::Resume | Decision::Retry => Kind::Primary,
            d if d.destructive() => Kind::Danger,
            _ => Kind::Default,
        };
        MenuItem::new(d.label(), ask(d)).kind(kind)
    };
    let items: Vec<MenuItem> = std::iter::once(&d.primary)
        .chain(&d.menu)
        .map(item)
        .collect();
    html! {
        <SplitButton {items} menu_label="More rollout actions" testid="rollout-controls" />
    }
}

/// The active rollout (`active`) when it wants an approval for a phase that
/// holds `target`: a host or cluster page puts reviewing it first.
#[hook]
pub(crate) fn use_approval_wanted(active: Option<String>, target: String) -> Option<String> {
    let rollout = use_fetch(active.clone(), |id| async move {
        match id {
            Some(id) => api::rollout(&id).await.map(Some),
            None => Ok(None),
        }
    });
    let live = use_live_value(&rollout, |d: &mut Option<RolloutDetail>, e| {
        if let Some(d) = d {
            apply_rollout(d, e)
        }
    });
    let d = live
        .flatten()
        .filter(|d| Some(&d.summary.id) == active.as_ref())?;
    let pipeline = layout(&d);
    wanting_approval(&pipeline)
        .iter()
        .any(|p| p.hosts.iter().any(|h| h.host == target))
        .then(|| d.summary.id.clone())
}

fn ask_for(d: &Decision) -> Ask {
    match d {
        Decision::Approve(phase) => Ask::Approve(phase.clone()),
        Decision::Reject => Ask::Reject,
        Decision::Pause => Ask::Action("pause"),
        Decision::Resume => Ask::Action("resume"),
        Decision::Retry => Ask::Action("retry"),
        Decision::SkipPhase => Ask::Action("skip-phase"),
        Decision::Expedite => Ask::Action("expedite"),
        Decision::Rollback => Ask::Action("rollback"),
        Decision::Abort => Ask::Action("abort"),
    }
}

/// The rollout's units (hosts and placements) in `state`.
fn hosts_in(d: &RolloutDetail, state: HostRolloutState) -> Vec<&RolloutHost> {
    d.phases
        .iter()
        .flat_map(|p| &p.hosts)
        .filter(|h| h.state == state)
        .collect()
}

/// The failed units a rollout-wide retry re-plans.
pub(crate) fn failed(d: &RolloutDetail) -> Vec<&RolloutHost> {
    hosts_in(d, HostRolloutState::Failed)
}

fn baking(d: &RolloutDetail) -> Vec<&RolloutHost> {
    hosts_in(d, HostRolloutState::Baking)
}

/// Phases an approval is wanted for: reached and waiting, or gated and not
/// yet approved, or holding hosts whose plan changed since.
pub(crate) fn wanting_approval<'a>(pipeline: &pipeline::Pipeline<'a>) -> Vec<&'a RolloutPhase> {
    pipeline
        .stages
        .iter()
        .filter(|st| {
            st.gate.kind == GateKind::Approval
                && (st.gate.state == GateState::AwaitingApproval
                    || (st.gate.state == GateState::Waiting && st.phase.approved_by.is_none())
                    || st
                        .hosts()
                        .any(|h| h.needs_approval && !finished_state(h.state)))
                && st.counts.total > 0
        })
        .map(|st| st.phase)
        .collect()
}

/// Whether an operator can force apply a host in this state: it is not
/// planning, applying or baking right now.
fn can_force_apply(s: HostRolloutState) -> bool {
    matches!(
        s,
        HostRolloutState::Held
            | HostRolloutState::Planned
            | HostRolloutState::Failed
            | HostRolloutState::Healthy
            | HostRolloutState::Skipped
            | HostRolloutState::Pending
    )
}

/// Whether a host in this state can be re-planned: its plan is not applied
/// yet (to plan, planned), or it failed, was held, skipped or missed its
/// phase.
fn can_replan(s: HostRolloutState) -> bool {
    matches!(
        s,
        HostRolloutState::PlanPending
            | HostRolloutState::Planned
            | HostRolloutState::Failed
            | HostRolloutState::Held
            | HostRolloutState::Skipped
            | HostRolloutState::Pending
    )
}

fn finished_state(s: HostRolloutState) -> bool {
    matches!(
        s,
        HostRolloutState::Healthy | HostRolloutState::Skipped | HostRolloutState::RolledBack
    )
}

/// The hosts of a phase an approval concerns: before the first approval,
/// every host not yet finished; after it, those whose plan changed since and
/// those still without a plan. Unchanged hosts count too: the approval gates
/// the phase, not only its changes, and must cover every plan in it.
fn approval_hosts(phase: &RolloutPhase) -> Vec<&RolloutHost> {
    let approved = phase.approved_by.is_some();
    phase
        .hosts
        .iter()
        .filter(|h| !finished_state(h.state))
        .filter(|h| !approved || h.needs_approval || h.plan_digest.is_none())
        .collect()
}

pub(crate) fn scope_of(phase: &RolloutPhase, plans: &[PlanGroup]) -> ApprovalScope {
    approval_scope(&approval_hosts(phase), plans)
}

/// The phases before `phase` that have not finished: an approval gate keeps
/// a change from rolling out blind, so a phase is approved only once these
/// are done (the server refuses it until then).
pub(crate) fn earlier_running<'a>(d: &'a RolloutDetail, phase: &RolloutPhase) -> Vec<&'a str> {
    let index = d
        .phases
        .iter()
        .position(|p| p.name == phase.name)
        .unwrap_or(0);
    d.phases[..index]
        .iter()
        .filter(|p| {
            !matches!(
                p.state,
                buckle_api::PhaseState::Done | buckle_api::PhaseState::Skipped
            )
        })
        .map(|p| p.name.as_str())
        .collect()
}

/// The phase an approval is for next: the one the rollout waits on, else
/// the first that wants one.
pub(crate) fn next_approval<'a>(wanting: &[&'a RolloutPhase]) -> Option<&'a RolloutPhase> {
    wanting
        .iter()
        .find(|p| p.state == buckle_api::PhaseState::AwaitingApproval)
        .or(wanting.first())
        .copied()
}

/// Whether `phase` can be approved now: the phases before it have finished
/// and it has plans to bind.
pub(crate) fn phase_approvable(
    d: &RolloutDetail,
    phase: &RolloutPhase,
    plans: &[PlanGroup],
) -> bool {
    earlier_running(d, phase).is_empty() && !scope_of(phase, plans).digests.is_empty()
}

/// The rollout's plans as one view across every phase. The next phase
/// wanting an approval puts its Approve button in the panel's head and its
/// notes above the plans; the plans are reviewed together, approved phase by
/// phase.
fn plans_panel(
    wanting: &[&RolloutPhase],
    d: &RolloutDetail,
    plans: &[PlanGroup],
    admin: bool,
    open: &Callback<Ask>,
    view: &UseStateHandle<AttrValue>,
) -> Html {
    if plans.is_empty() && wanting.is_empty() {
        return html! {};
    }
    let phases = Phases(
        d.phases
            .iter()
            .map(|p| (p.name.clone(), p.hosts.iter().map(units::key).collect()))
            .collect(),
    );
    let clusters: Vec<String> = d
        .phases
        .iter()
        .flat_map(|p| &p.hosts)
        .filter(|h| h.kind != buckle_api::TargetKind::Host)
        .map(units::key)
        .collect();
    let in_phases = d.phases.iter().filter(|p| !p.hosts.is_empty()).count();
    let mut subtitle = format!(
        "{} across {}",
        count(plans.len(), "distinct plan", "distinct plans"),
        count(in_phases, "phase", "phases")
    );
    if !wanting.is_empty() {
        let names: Vec<&str> = wanting.iter().map(|p| p.name.as_str()).collect();
        subtitle.push_str(&format!(" · approval wanted: {}", names.join(", ")));
    }
    let (approve, notes) = next_approval(wanting)
        .map(|p| approval(p, d, plans, admin, open))
        .unwrap_or_default();
    // The toggle only where there is a union to switch from.
    let union = d.fleet.as_ref().is_some_and(|f| !f.hosts.is_empty());
    let by_plan = !union || **view == "by-target";
    let actions = html! { <>
        if union && !plans.is_empty() {
            <Segmented label="Plans shown" testid="plans-view" value={(**view).clone()}
                options={vec![("combined".into(), "Combined".into()), ("by-target".into(), "By target".into())]}
                on_change={{ let view = view.clone(); Callback::from(move |v| view.set(v)) }} />
        }
        { approve }
    </> };
    html! {
        <Panel title="Plans" id="plans" {subtitle} {actions}
            class={classes!((!wanting.is_empty()).then_some("panel--approval"))}>
            { notes }
            if !plans.is_empty() {
                <PlanViews fleet={d.fleet.clone()} groups={plans.to_vec()} {phases}
                    open_destructive={!wanting.is_empty()} clusters={clusters} {by_plan} />
            }
        </Panel>
    }
}

/// One phase's approval: its button (for the panel's head, disabled until
/// the phases before it finish) and its notes, on what it binds and leaves
/// out.
fn approval(
    phase: &RolloutPhase,
    d: &RolloutDetail,
    plans: &[PlanGroup],
    admin: bool,
    open: &Callback<Ask>,
) -> (Html, Html) {
    let scope = scope_of(phase, plans);
    let running = earlier_running(d, phase);
    // A cluster, or a step after one, is planned once its stage opens, so it
    // has no plan before then by design.
    let (deferred, missing): (Vec<&String>, Vec<&String>) =
        scope.without_plan.iter().partition(|name| {
            phase.hosts.iter().any(|h| {
                units::key(h) == **name && (h.kind != buckle_api::TargetKind::Host || h.step > 0)
            })
        });
    let names = |v: &[&String]| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
    let policy = if phase.approval == "if_destructive" {
        "if_destructive, and a plan here is destructive"
    } else {
        "required"
    };
    let why_not = if !running.is_empty() {
        Some(format!(
            "{} can be approved once {} {} finished.",
            phase.name,
            running.join(", "),
            if running.len() == 1 { "has" } else { "have" }
        ))
    } else if scope.digests.is_empty() {
        Some("No plans to approve yet.".to_string())
    } else {
        None
    };
    let title = why_not.clone().unwrap_or_else(|| {
        format!(
            "Binds {} for {} (approval: {policy}). A host whose plan changes after approval asks again.",
            count(scope.digests.len(), "plan digest", "plan digests"),
            count(scope.hosts.len(), "host", "hosts")
        )
    });
    let button = if !admin {
        html! { <span class="muted small">{ "Approving needs an administrator (admin_acl)." }</span> }
    } else {
        let (open, name) = (open.clone(), phase.name.clone());
        html! {
            <span class="approval__button" title={title} data-testid="approve-phase">
                <Button kind={Kind::Primary} disabled={why_not.is_some()} label={format!("Approve phase {}…", phase.name)}
                    onclick={Callback::from(move |_: MouseEvent| open.emit(Ask::Approve(name.clone())))}>
                    { format!("Approve {}…", phase.name) }
                </Button>
            </span>
        }
    };
    let notes = html! { <>
        if !running.is_empty() {
            <Note testid="approval-waits">
                { format!("{} still running: {} can be approved once {} finished. Review its plans meanwhile.",
                    running.join(", "), phase.name, if running.len() == 1 { "it has" } else { "they have" }) }
            </Note>
        }
        if scope.groups.is_empty() {
            if missing.is_empty() && !deferred.is_empty() {
                <Note>{ format!("No plans for {} yet: {} plans when its stage opens; the phase can be approved once it has.", phase.name, names(&deferred)) }</Note>
            } else {
                <Note>{ format!("No plans for {} yet: plans arrive as the hosts answer; the phase can be approved once they have.", phase.name) }</Note>
            }
        }
        if !missing.is_empty() {
            <Note kind={NoteKind::Warn}>
                { format!("{} in {} {} no plan yet ({}): not covered by its approval. A host that misses its wave moves to the stragglers stage.",
                    count(missing.len(), "host", "hosts"), phase.name, if missing.len() == 1 { "has" } else { "have" }, names(&missing)) }
            </Note>
        }
        if !deferred.is_empty() && !scope.groups.is_empty() {
            <Note>
                { format!("Planned when its stage opens, so not covered by the approval of {}: {}.", phase.name, names(&deferred)) }
            </Note>
        }
    </> };
    (button, notes)
}

fn dialog(
    ask: &Ask,
    d: &RolloutDetail,
    plans: &[PlanGroup],
    busy: bool,
    error: Option<String>,
    run: &Callback<(Ask, String)>,
    close: &Callback<()>,
) -> Html {
    let confirm = {
        let (run, ask) = (run.clone(), ask.clone());
        Callback::from(move |note: String| run.emit((ask.clone(), note)))
    };
    let name = rollout_label(&d.summary.id);
    match ask {
        Ask::Bind(..) => html! {},
        Ask::Approve(phase_name) => {
            let Some(phase) = d.phases.iter().find(|p| &p.name == phase_name) else {
                return html! {};
            };
            let scope = scope_of(phase, plans);
            // What is sent is what this dialog shows: if the plans change
            // while it is open, it redraws with the new digests.
            let confirm = {
                let (run, bind) = (
                    run.clone(),
                    Ask::Bind(phase_name.clone(), scope.digests.clone()),
                );
                Callback::from(move |note: String| run.emit((bind.clone(), note)))
            };
            let hosts = scope.hosts.len();
            let ack = (scope.destructive > 0).then(|| {
                AttrValue::from(format!(
                    "I have reviewed the {} destructive {}, including {}.",
                    scope.destructive,
                    if scope.destructive == 1 {
                        "step"
                    } else {
                        "steps"
                    },
                    count(scope.removals, "removal", "removals"),
                ))
            });
            html! {
                <Confirm title={format!("Approve {phase_name} of rollout {name}")}
                    confirm_label={format!("Approve {}", count(hosts, "host", "hosts"))}
                    acknowledge={ack} note=true note_placeholder="Why this is safe to go ahead"
                    {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                    <p>{ format!("This approves {} on {} in {phase_name}.", count(scope.digests.len(), "distinct plan", "distinct plans"), count(hosts, "host", "hosts")) }</p>
                    <ul class="confirm__facts" data-testid="approval-summary">
                        <li><strong>{ hosts }</strong>{ " hosts: " }<span class="mono">{ scope.hosts.join(", ") }</span></li>
                        <li><strong>{ scope.changes }</strong>{ " changes across the distinct plans" }</li>
                        <li class={classes!((scope.destructive > 0).then_some("text-danger"))}>
                            <strong>{ scope.destructive }</strong>{ " destructive steps (" }{ count(scope.removals, "removal", "removals") }{ ")" }
                        </li>
                        <li>{ "Binds digests " }{ for scope.digests.iter().map(|g| html! { <code class="mono">{ format!("{} ", short(g)) }</code> }) }</li>
                    </ul>
                    if !scope.without_plan.is_empty() {
                        <p class="muted">{ format!("Not covered (no plan yet): {}.", scope.without_plan.join(", ")) }</p>
                    }
                    <p class="muted small">{ "The approval binds these digests: an agent refuses to apply any other plan, and a host whose plan changes asks again." }</p>
                </Confirm>
            }
        }
        Ask::Reject => html! {
            <Confirm title={format!("Reject rollout {name}")} confirm_label="Reject rollout" danger=true note_required=true
                note_placeholder="What is wrong with these plans"
                type_to_confirm={AttrValue::from(name.clone())} {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                <p>{ "Rejecting ends the rollout here: nothing in the waiting phase is applied, and the rollout is recorded as aborted with your reason. Hosts updated in earlier phases keep the new revision until a later rollout or a rollback changes them." }</p>
            </Confirm>
        },
        Ask::Action(action) => {
            let (failed, baking) = (failed(d), baking(d));
            let phase = d
                .summary
                .current_phase
                .clone()
                .unwrap_or_else(|| "the phase".into());
            let (title, label, danger, body, typed) = match *action {
                "pause" => ("Pause", "Pause rollout", false, "Nothing new starts while paused; hosts mid-apply finish their apply.".to_string(), None),
                "resume" => ("Resume", "Resume rollout", false, "The rollout carries on from where it paused.".to_string(), None),
                "retry" => ("Retry", "Retry", false, format!(
                    "Re-plans and re-applies {}; the same checks apply, and anything whose plan changed asks for approval again. A rollout paused on these failures carries on once they are cleared.",
                    units::tally(&failed, "failed", true)), None),
                "skip-phase" => ("Skip this phase of", "Skip this phase", true, format!(
                    "Accepts {} in {phase} as failures and carries on to the next phase without them. They stay failed in this rollout until retried from their rows.",
                    units::tally(&failed, "failed", false)), None),
                "expedite" => ("Expedite", "Expedite rollout", true, format!(
                    "Counts {} as baked now, without waiting out the rest of the bake window. Anything whose checks are failing keeps baking.",
                    units::tally(&baking, "baking", true)), None),
                "rollback" => ("Roll back", "Roll back", true, format!(
                    "Applies the previous stable revision to everything this rollout updated ({}), newest phase first. Rolling back is applying, including removal of what this revision added; an irreversible task rolls back only as far as its kind can.",
                    units::tally(&updated(d), "", false)), Some(name.clone())),
                _ => ("Abort", "Abort rollout", true, "Stops the rollout where it is, with its state recorded. Hosts already updated keep the new revision.".to_string(), Some(name.clone())),
            };
            html! {
                <Confirm title={format!("{title} rollout {name}")} confirm_label={label} {danger} note=true
                    type_to_confirm={typed.map(AttrValue::from)} {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                    <p>{ body }</p>
                </Confirm>
            }
        }
        Ask::Host(action, host) => {
            let retry_label = d
                .phases
                .iter()
                .flat_map(|p| &p.hosts)
                .find(|h| units::key(h) == *host)
                .map(|h| replan_label(h.state, h.plan_digest.is_some()))
                .unwrap_or("Re-plan");
            let (label, danger, body) = match *action {
                "retry" => (
                    retry_label,
                    false,
                    "Plans the host again as it stands. A host that failed, was held or skipped is then re-applied with the same checks; one still waiting to apply keeps its place and its plan asks for approval again if it changed.",
                ),
                "expedite" => (
                    "Expedite",
                    true,
                    "Counts the host as baked now, without waiting out the rest of its bake window. Refused while one of its checks is failing.",
                ),
                "redeploy" => (
                    "Redeploy",
                    false,
                    "Applies the host's plan again now, without approval or a digest check, then bakes it like any other host. Use it to re-run a deploy that already finished. Your note is recorded in the audit log.",
                ),
                "force" => (
                    "Force apply",
                    true,
                    "Applies whatever the host plans right now, without approval, a digest check or a passing baseline. Use it to re-run a plan, to run one on a host that failed it, or when the plan keeps changing between attempts. Your note is recorded in the audit log.",
                ),
                _ => (
                    "Skip",
                    true,
                    "Excludes the host from this rollout; it converges later as a straggler or with the next rollout.",
                ),
            };
            let confirm_label = format!("{label} {host}");
            html! {
                <Confirm title={format!("{label} {host}")} {confirm_label} {danger} note=true
                    note_placeholder={if *action == "skip" { "Reason (recorded with the skip)" } else { "" }}
                    {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                    <p>{ body }</p>
                </Confirm>
            }
        }
        Ask::Phase(_, phase) => {
            let baking: Vec<String> = d
                .phases
                .iter()
                .filter(|p| p.name == *phase)
                .flat_map(|p| &p.hosts)
                .filter(|h| h.state == HostRolloutState::Baking)
                .map(units::key)
                .collect();
            html! {
                <Confirm title={format!("Expedite {phase}")} confirm_label={format!("Expedite {phase}")} danger=true note=true
                    {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                    <p>{ format!(
                        "Counts {} in {phase} ({}) as baked now, without waiting out the rest of their bake windows. A host whose checks are failing keeps baking.",
                        count(baking.len(), "baking host", "baking hosts"), baking.join(", ")) }</p>
                </Confirm>
            }
        }
        Ask::Continue(host, checks) => html! {
            <Confirm title={format!("Continue {host} with checks waived")} confirm_label={format!("Waive {} and continue", count(checks.len(), "check", "checks"))}
                danger=true note=true acknowledge={AttrValue::from("I understand these checks still run and report, but no longer count for this host in this rollout.")}
                {busy} {error} on_confirm={confirm} on_cancel={close.clone()}>
                <p>{ "Retries the host with these failing checks waived, so the run proceeds past the module and finishes the rest:" }</p>
                <ul class="confirm__facts">{ for checks.iter().map(|c| html! { <li class="mono">{ c.clone() }</li> }) }</ul>
            </Confirm>
        },
    }
}

fn updated(d: &RolloutDetail) -> Vec<&RolloutHost> {
    d.phases
        .iter()
        .flat_map(|p| &p.hosts)
        .filter(|h| {
            matches!(
                h.state,
                HostRolloutState::Healthy
                    | HostRolloutState::Baking
                    | HostRolloutState::Rebooting
                    | HostRolloutState::Applying
                    | HostRolloutState::Failed
            )
        })
        .collect()
}

/// The rollout's own events, newest first, each at its time from the start.
fn timeline(events: Vec<&RolloutEvent>, t0: DateTime<Utc>) -> Html {
    if events.is_empty() {
        return html! { <p class="muted">{ "No events yet." }</p> };
    }
    html! {
        <ol class="timeline">
            { for events.into_iter().rev().map(|e| html! {
                <li class={classes!("timeline__item", tone_of_event(&e.kind).class())}>
                    <time class="timeline__at mono" datetime={e.at.to_rfc3339()} title={timestamp(e.at)}>{ since(e.at, t0) }</time>
                    <span class="timeline__kind mono">{ e.kind.clone() }</span>
                    <span class="timeline__message">{ e.message.clone() }</span>
                    if let Some(a) = &e.actor { <span class="timeline__actor muted">{ a.clone() }</span> }
                </li>
            }) }
        </ol>
    }
}

fn tone_of_event(kind: &str) -> Tone {
    if kind.contains("fail") || kind.contains("abort") {
        Tone::Danger
    } else if kind.contains("rollback") {
        Tone::Mute
    } else if kind.contains("held")
        || kind.contains("paused")
        || kind.contains("check_failing")
        || kind.contains("lost")
        || kind.contains("skip")
    {
        Tone::Warn
    } else if kind.contains("healthy")
        || kind.contains("done")
        || kind.contains("approved")
        || kind.contains("complete")
    {
        Tone::Ok
    } else {
        Tone::Neutral
    }
}

/// A host's lifecycle ticker: its states as segments of a bar, a dashed
/// remainder for a bake or reboot still under way, and its events as markers above
/// the bar, each with the whole event in a popover.
fn ticker_view(t: &Ticker<'_>, t0: DateTime<Utc>, now: DateTime<Utc>) -> Html {
    let segments = t.segments.iter().map(|seg| {
        let (left, right) = (t.pct(seg.from), t.pct(seg.until));
        let label = host_state_label(seg.state);
        let span = format!("{label}: {} to {}", since(seg.from, t0), if seg.ongoing { "now".to_string() } else { since(seg.until, t0) });
        html! {
            <span class={classes!("ticker__seg", host_state_tone(seg.state).class(), seg.ongoing.then_some("ticker__seg--ongoing"))}
                style={format!("left: {left:.2}%; width: {:.2}%", (right - left).max(0.4))}
                data-state={label.replace(' ', "-")} title={span}>
                <span class="ticker__label">{ label }</span>
            </span>
        }
    });
    let planned = t.planned.map(|(state, until)| {
        let (left, right) = (t.pct(now), t.pct(until));
        let title = match state {
            HostRolloutState::Rebooting => format!("must be back by {}", since(until, t0)),
            _ => format!("bake ends at {}", since(until, t0)),
        };
        html! {
            <span class={classes!("ticker__seg", "ticker__seg--planned", host_state_tone(state).class())}
                style={format!("left: {left:.2}%; width: {:.2}%", right - left)}
                {title} />
        }
    });
    let markers = t.markers.iter().map(|m| {
        let e = m.event;
        let at = t.pct(e.at);
        // The popover opens away from the nearer edge, so it stays in view.
        let side = if at < 35.0 { "start" } else if at > 65.0 { "end" } else { "middle" };
        html! {
            <span class={classes!("ticker__marker", format!("ticker__marker--{side}"), m.transition.is_some().then_some("ticker__marker--transition"), tone_of_event(&e.kind).class())}
                style={format!("left: {at:.2}%; --lane: {}", m.lane)} tabindex="0" role="listitem"
                data-kind={e.kind.clone()} aria-label={format!("{} at {}: {}", e.kind, since(e.at, t0), e.message)}>
                <span class="ticker__pop" role="tooltip">
                    <span class="ticker__pop-head">
                        <span class="mono ticker__kind">{ e.kind.clone() }</span>
                        <time class="mono muted" datetime={e.at.to_rfc3339()}>{ since(e.at, t0) }</time>
                    </span>
                    <span class="ticker__message">{ e.message.clone() }</span>
                    <span class="muted small">
                        { timestamp(e.at) }
                        if let Some(p) = &e.phase { { format!(" · phase {p}") } }
                        if let Some(a) = &e.actor { { format!(" · {a}") } }
                    </span>
                </span>
            </span>
        }
    });
    html! {
        <div class="ticker" data-testid="host-ticker" style={format!("--lanes: {}", t.lanes().max(1))}>
            <div class="ticker__markers" role="list" aria-label="Events">{ for markers }</div>
            <div class="ticker__bar" aria-hidden="true">
                { for segments }
                { planned.unwrap_or_default() }
            </div>
            <div class="ticker__axis muted small">
                <time class="mono" datetime={t.from.to_rfc3339()} title={timestamp(t.from)}>{ since(t.from, t0) }</time>
                <time class="mono" datetime={t.until.to_rfc3339()} title={timestamp(t.until)}>{ since(t.until, t0) }</time>
            </div>
        </div>
    }
}

/// A host's state pill's popover: how it stands on each check of its
/// phase's health gate, and the headline of its error.
fn health_tip(phase: &RolloutPhase, h: &RolloutHost) -> Html {
    let checks = host_checks(phase, h);
    let headline = host_error_headline(h);
    if checks.is_empty() && headline.is_none() {
        return html! {};
    }
    html! { <>
        if let Some(headline) = headline { <span>{ headline }</span> }
        if !checks.is_empty() {
            <span class="tip__head">{ format!("Health gate of {}", phase.name) }</span>
            <ul class="tip-checks" data-testid="host-health-tip">
                { for checks.into_iter().map(|(check, standing)| html! {
                    <li data-check={check.clone()}>
                        <Pill tone={standing.tone()} label={standing.label()} />
                        <span class="mono">{ check }</span>
                    </li>
                }) }
            </ul>
        }
    </> }
}

#[derive(Properties, PartialEq)]
struct HostTableProps {
    detail: RolloutDetail,
    /// The rollout's distinct plans, for what each host's changes.
    plans: Vec<PlanGroup>,
    admin: bool,
    on_ask: Callback<Ask>,
    /// Runs a decision without a dialog.
    on_run: Callback<Ask>,
    /// The module and step each applying host is on.
    activity: BTreeMap<String, String>,
    /// The fleet's hosts, for each name's hover card.
    hosts: BTreeMap<String, HostSummary>,
    /// Every unit, else only those that change something (or went wrong).
    all: bool,
    now: DateTime<Utc>,
}

/// A stage's rows, split by dependency step where it has more than one.
fn step_groups<'a>(
    st: &pipeline::Stage<'a>,
    all: bool,
) -> Vec<(Option<u32>, Vec<&'a RolloutHost>, usize)> {
    let mut steps: Vec<u32> = st.hosts().map(|h| h.step).collect();
    steps.sort_unstable();
    steps.dedup();
    let split = steps.len() > 1;
    let units: Vec<&'a RolloutHost> = st
        .targets
        .iter()
        .copied()
        .chain(st.clusters.iter().flat_map(|c| c.units.iter().copied()))
        .collect();
    let group = |of: Vec<&'a RolloutHost>| {
        let hidden = of
            .iter()
            .filter(|h| !all && !units::changed_view(h))
            .count();
        let shown: Vec<_> = of
            .into_iter()
            .filter(|h| all || units::changed_view(h))
            .collect();
        (shown, hidden)
    };
    if !split {
        let (shown, hidden) = group(units);
        return vec![(None, shown, hidden)];
    }
    steps
        .into_iter()
        .map(|s| {
            let (shown, hidden) = group(units.iter().copied().filter(|h| h.step == s).collect());
            (Some(s), shown, hidden)
        })
        .collect()
}

#[function_component(HostTable)]
fn host_table(props: &HostTableProps) -> Html {
    let expanded = use_state(|| None::<String>);
    let p = layout(&props.detail);
    let all = props.all;
    let groups = p.stages.iter().filter(|st| st.counts.total > 0).flat_map(|st| {
        let phase = st.phase;
        let c = st.counts.clone();
        // Said once on the stage's heading instead of on each of its rows.
        let all_need_approval = c.needs_approval == c.total;
        let baking = props.admin && st.hosts().any(|h| h.state == HostRolloutState::Baking);
        let on_ask = props.on_ask.clone();
        step_groups(st, all).into_iter().enumerate().map(move |(i, (step, rows, hidden))| {
            let first = i == 0;
            let key = match step {
                Some(s) => format!("stage-{}-{s}", phase.name),
                None => format!("stage-{}", phase.name),
            };
            let head = html! {
                <tr class="table__group" {key} data-stage={phase.name.clone()} data-step={step.map(|s| s.to_string())}>
                    <th colspan="7" scope="colgroup">
                        <span class="group-head">
                            <span class="group-head__name">{ match step {
                                Some(s) => format!("{} · step {}", phase.name, s + 1),
                                None => phase.name.clone(),
                            } }</span>
                            if first {
                                <Pill tone={phase_state_tone(phase.state)} label={phase_state_label(phase.state)} />
                                <Meter segments={stage_segments(&c)} total={c.total} />
                                <span class="muted small">{ format!("{}/{} done", c.healthy, c.total) }</span>
                                if all_need_approval { <span class="badge badge--info">{ "needs approval" }</span> }
                            }
                            if hidden > 0 {
                                <span class="muted small" data-testid="hidden-unchanged">{ format!("{hidden} unchanged not shown") }</span>
                            }
                            if first && baking {
                                <span class="group-head__controls">
                                    <SplitButton items={stage_menu(&phase.name, &on_ask)} small=true menu_label={format!("More actions for {}", phase.name)} testid="stage-actions" />
                                </span>
                            }
                        </span>
                    </th>
                </tr>
            };
            (head, rows.into_iter().map(move |h| (phase, h, all_need_approval)))
        }).collect::<Vec<_>>()
    });
    html! {
        <div class="table-wrap">
            <table class="table table--hosts">
                <thead><tr>
                    <th scope="col">{ "Target" }</th>
                    <th scope="col">{ "Wave" }</th>
                    <th scope="col">{ "State" }</th>
                    <th scope="col">{ "Plan" }</th>
                    <th scope="col">{ "Checks" }</th>
                    <th scope="col">{ "Updated" }</th>
                    <th scope="col"><span class="visually-hidden">{ "Details" }</span></th>
                </tr></thead>
                <tbody>
                    { for groups.map(|(head, rows)| html! { <>
                    { head }
                    { for rows.map(|(phase, h, all_need_approval)| {
                        let unit = units::key(h);
                        let open = expanded.as_deref() == Some(unit.as_str());
                        let toggle = {
                            let (expanded, name) = (expanded.clone(), unit.clone());
                            Callback::from(move |_: MouseEvent| expanded.set(if open { None } else { Some(name.clone()) }))
                        };
                        // A click anywhere on the row opens it, except on
                        // what is a control of its own (the host's link, a
                        // popover's trigger, the toggle itself).
                        let on_row = {
                            let toggle = toggle.clone();
                            Callback::from(move |e: MouseEvent| {
                                let own = e
                                    .target_dyn_into::<web_sys::Element>()
                                    .and_then(|t| t.closest("a, button, .tip, input, select, textarea").ok().flatten())
                                    .is_some();
                                if !own {
                                    toggle.emit(e);
                                }
                            })
                        };
                        html! { <>
                            <tr key={unit.clone()} class={classes!("table__toggle-row", open.then_some("is-open"))} onclick={on_row}
                                data-unit={unit.clone()} data-unchanged={units::unchanged(h).then_some("true")}>
                                <td>{ unit_cell(h, props.hosts.get(&h.host), props.now) }</td>
                                <td class="muted">{ h.wave.map(|w| w.to_string()).unwrap_or_else(|| "—".into()) }</td>
                                <td>
                                    <HoverTip tip={health_tip(phase, h)} label={format!("{}: {}", units::name(h), host_state_label(h.state))}>
                                        <Pill tone={host_state_tone(h.state)} label={host_state_label(h.state)} />
                                    </HoverTip>
                                    if h.needs_approval && !all_need_approval { <span class="badge badge--info">{ "needs approval" }</span> }
                                    if h.needs_attention {
                                        <span class="badge badge--danger" title="The host's own rollback failed: it may be left on a mix of revisions. See the error below.">{ "needs attention" }</span>
                                    }
                                    if let Some(until) = h.bake_until.filter(|u| h.state == HostRolloutState::Baking && *u > props.now) {
                                        <span class="small muted">{ " " }<Countdown {until} /></span>
                                    }
                                    if let Some(r) = awaited_reboot(h, props.now) {
                                        <span class="small muted" title={format!("fails unless back by {}", timestamp(r.until))}>{ " back within " }<Countdown until={r.until} /></span>
                                    }
                                    if let Some(now) = props.activity.get(&unit).filter(|_| h.state == HostRolloutState::Applying) {
                                        <span class="cell-sub mono" data-testid="host-activity">{ now.clone() }</span>
                                    }
                                    if let Some(w) = crate::logic::clusters::waiting(phase, h) {
                                        <span class="cell-sub" data-testid="host-waiting">{ w.describe() }</span>
                                    } else if h.kind != buckle_api::TargetKind::Host && let Some(e) = &h.executor {
                                        <span class="cell-sub">{ "via " }<span class="mono">{ e.clone() }</span></span>
                                    }
                                </td>
                                <td>
                                    <span class="nowrap">
                                        if units::unchanged(h) {
                                            <span class="muted small">{ "unchanged" }</span>
                                        } else {
                                            { host_tally(h, &props.plans).map(|tally| html! { <Changes {tally} /> }).unwrap_or_default() }
                                        }
                                        { h.plan_digest.as_deref().map(|d| html! {
                                            <span class="mono muted small plan-digest" title={format!("plan {d}")}>{ short(d) }</span>
                                        }).unwrap_or_else(|| html! { <span class="muted">{ "—" }</span> }) }
                                    </span>
                                    if h.preview {
                                        <span class="badge" data-testid="plan-preview"
                                            title="Planned at the rollout's start; planned again when its stage opens">{ "preview" }</span>
                                    }
                                    if let Some(from) = &h.drifted_from {
                                        <span class="badge badge--warn" data-testid="plan-drifted"
                                            title={format!("The plan changed since its preview ({})", short(from))}>{ "changed since preview" }</span>
                                    }
                                </td>
                                <td>
                                    { for h.failed_checks.iter().map(|c| html! { <span class="badge badge--danger mono">{ c.clone() }</span> }) }
                                    { for h.waived_checks.iter().map(|c| html! { <span class="badge mono" title="waived for this rollout">{ format!("waived {c}") }</span> }) }
                                </td>
                                <td title={timestamp(h.updated_at)}>{ relative(h.updated_at, props.now) }</td>
                                <td class="row-controls">
                                    if props.admin {
                                        <SplitButton items={host_menu(h, &props.on_ask)} small=true menu_label={format!("More actions for {}", units::name(h))} testid="host-actions" />
                                    }
                                    <button type="button" class="icon-btn row-toggle" onclick={toggle}
                                        aria-expanded={open.to_string()} aria-label={format!("Details for {}", units::name(h))}
                                        title={if open { "Hide details" } else { "Show details" }}>
                                        { icons::chevron() }
                                    </button>
                                </td>
                            </tr>
                            if open {
                                <tr class="table__detail" key={format!("{unit}-detail")}>
                                    <td colspan="7">
                                        <HostDetailRow detail={props.detail.clone()} host={h.clone()} admin={props.admin} on_run={props.on_run.clone()} now={props.now} />
                                    </td>
                                </tr>
                            }
                        </> }
                    }) }
                    </> }) }
                </tbody>
            </table>
        </div>
    }
}

/// A stage's own actions, on its heading in the Targets card: expedite its
/// baking rows (end their bakes now).
fn stage_menu(phase: &str, on_ask: &Callback<Ask>) -> Vec<MenuItem> {
    let (on_ask, a) = (on_ask.clone(), Ask::Phase("expedite", phase.to_owned()));
    vec![MenuItem::new(
        "Expedite…",
        Callback::from(move |_: MouseEvent| on_ask.emit(a.clone())),
    )]
}

/// A host's own decisions, for the menu on its row.
/// A host's own actions in a rollout, the most likely first (the split
/// button's face): retry what failed, continue past its failed checks, skip
/// expedite its bake, or redeploy a host already done, then the ones that
/// override the rollout. A face that overrides the rollout (Skip on a host
/// still applying) is not drawn red: the dialog says what it does.
fn host_menu(h: &RolloutHost, on_ask: &Callback<Ask>) -> Vec<MenuItem> {
    let item = |label: &str, a: Ask| {
        let on_ask = on_ask.clone();
        MenuItem::new(
            label.to_string(),
            Callback::from(move |_: MouseEvent| on_ask.emit(a.clone())),
        )
    };
    let host = || units::key(h);
    let mut items = Vec::new();
    if can_replan(h.state) {
        let label = format!("{}…", replan_label(h.state, h.plan_digest.is_some()));
        items.push(item(&label, Ask::Host("retry", host())));
    }
    if h.state == HostRolloutState::Failed && !h.failed_checks.is_empty() {
        items.push(item(
            "Continue with checks waived…",
            Ask::Continue(host(), h.failed_checks.clone()),
        ));
    }
    if h.state == HostRolloutState::Baking {
        items.push(item("Expedite…", Ask::Host("expedite", host())));
    }
    if h.state == HostRolloutState::Healthy {
        // Done: applying its plan again is the one thing left to ask.
        items.push(item("Redeploy…", Ask::Host("redeploy", host())));
    } else if can_force_apply(h.state) {
        items.push(item("Force apply…", Ask::Host("force", host())).danger());
    }
    if !finished_state(h.state) {
        items.push(item("Skip…", Ask::Host("skip", host())).danger());
    }
    if let Some(face) = items.first_mut() {
        face.kind = Kind::Default;
    }
    items
}

#[derive(Properties, PartialEq)]
struct HostDetailRowProps {
    detail: RolloutDetail,
    host: RolloutHost,
    admin: bool,
    on_run: Callback<Ask>,
    now: DateTime<Utc>,
}

#[function_component(HostDetailRow)]
fn host_detail_row(props: &HostDetailRowProps) -> Html {
    let h = &props.host;
    let rollout_id = props.detail.summary.id.clone();
    let started = props.detail.summary.started_at;
    let events: Vec<&RolloutEvent> = props
        .detail
        .events
        .iter()
        .filter(|e| e.host.as_deref() == Some(h.host.as_str()) && e.workload == h.workload)
        .collect();
    // The host's run in this rollout, once it has one.
    let has_run = h.run_id.is_some()
        || matches!(
            h.state,
            HostRolloutState::Applying
                | HostRolloutState::Rebooting
                | HostRolloutState::Baking
                | HostRolloutState::Healthy
                | HostRolloutState::Failed
                | HostRolloutState::RolledBack
        );
    let run = use_run_report(
        has_run.then(|| RunSource::Rollout {
            rollout: rollout_id.clone(),
            host: units::key(h),
            run: h.run_id.clone(),
        }),
        None,
    );
    // A target planned again (a retry, a force apply, its next step) keeps
    // what its earlier attempts applied: each one's run can be shown in
    // place of the latest.
    let attempts = attempt_options(h);
    let chosen = use_state(|| AttrValue::from("latest"));
    let earlier = chosen.parse::<usize>().ok().and_then(|i| h.earlier.get(i));
    let earlier_run = use_run_report(
        earlier
            .and_then(|a| a.run_id.clone())
            .map(|run| RunSource::Host {
                host: h.host.clone(),
                run,
            }),
        None,
    );
    // A host with an error is retried (or re-planned) from it: this one, or
    // (when it failed) every failed host of the rollout at once.
    let replan = (props.admin && can_replan(h.state)).then(|| {
        let others = failed(&props.detail).len();
        let label = replan_label(h.state, h.plan_digest.is_some());
        let run = |a: Ask| {
            let on_run = props.on_run.clone();
            Callback::from(move |_: MouseEvent| on_run.emit(a.clone()))
        };
        html! {
            <div class="error-block__actions">
                <SplitButton small=true testid="host-replan" menu_label="More ways to retry" items={
                    std::iter::once(MenuItem::new(label, run(Ask::Host("retry", units::key(h)))).kind(Kind::Primary))
                        .chain((others > 1 && h.state == HostRolloutState::Failed).then(||
                            MenuItem::new(format!("Retry all {others} failed"), run(Ask::Action("retry")))))
                        .collect::<Vec<_>>()
                } />
            </div>
        }
    });
    let lifecycle = ticker(Source {
        events,
        state: h.state,
        run_started: run.report.as_ref().and_then(|r| r.started_at),
        run_finished: run.report.as_ref().and_then(|r| r.finished_at),
        bake_until: h.bake_until,
        reboot_until: h.reboot.as_ref().map(|r| r.until),
        now: props.now,
    });
    html! {
        <div class="host-row">
            <ErrorBlock error={h.error_detail.clone()} raw={h.error.clone().map(AttrValue::from)} tone={host_error_tone(h)}
                actions={replan.unwrap_or_default()} />
            <div class="host-row__ticker">
                <h4 class="subhead">{ "Through this rollout" }</h4>
                { match &lifecycle {
                    Some(t) => ticker_view(t, started, props.now),
                    None => html! { <p class="muted">{ "Nothing has happened to this host in this rollout yet." }</p> },
                } }
            </div>
            <div class="host-row__run" data-testid="rollout-host-run">
                <div class="host-row__run-head">
                    <h4 class="subhead">{ if attempts.is_empty() { "Run in this rollout" } else { "Runs in this rollout" } }</h4>
                    if !attempts.is_empty() {
                        <Segmented label="Attempt shown" testid="host-attempts" value={(*chosen).clone()}
                            options={attempts.iter().map(|(v, l)| (AttrValue::from(v.clone()), AttrValue::from(l.clone()))).collect::<Vec<_>>()}
                            on_change={{ let chosen = chosen.clone(); Callback::from(move |v| chosen.set(v)) }} />
                    }
                </div>
                if let Some(a) = earlier {
                    <p class="small muted" data-testid="host-attempt">
                        { format!("{} · planned again {} · ", host_state_label(a.state), relative(a.at, props.now)) }
                        { attempt_changes(a) }
                        <span class="mono plan-digest" title={format!("plan {}", a.digest)}>{ short(&a.digest) }</span>
                    </p>
                    if let Some(e) = &a.error { <p class="small" data-testid="host-attempt-error">{ e.clone() }</p> }
                    { match (&earlier_run.report, &earlier_run.error) {
                        (Some(report), _) => html! { <RunReportView report={report.clone()} in_rollout=true workloads={props.host.kind != buckle_api::TargetKind::Host} /> },
                        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={earlier_run.reload.clone()} /> },
                        (None, None) if a.run_id.is_none() || earlier_run.settled => html! { <p class="muted">{ "The server has no run report for this attempt." }</p> },
                        (None, None) => html! { <Loading label="Loading the run…" /> },
                    } }
                } else {
                { match (&run.report, &run.error) {
                    (Some(report), _) => html! { <RunReportView report={report.clone()} in_rollout=true workloads={props.host.kind != buckle_api::TargetKind::Host} /> },
                    (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={run.reload.clone()} /> },
                    (None, None) if !has_run => html! { <p class="muted">{ "No run yet: the host has not started applying in this rollout." }</p> },
                    (None, None) if run.settled => html! { <p class="muted">{ "The server has no run report for this host in this rollout." }</p> },
                    (None, None) => html! { <Loading label="Loading the run…" /> },
                } }
                }
            </div>
        </div>
    }
}

/// What an earlier attempt's plan changed, as its host counted it.
fn attempt_changes(a: &buckle_api::RolloutAttempt) -> String {
    match (a.changes, a.removals.unwrap_or(0)) {
        (Some(0), 0) => "no changes · ".to_owned(),
        (Some(c), _) => format!("{} · ", count(c as usize, "change", "changes")),
        (None, _) => String::new(),
    }
}

/// A Targets row's name: a host's hover card, or a workload linking to its
/// page with the cluster it runs on as a muted suffix.
fn unit_cell(h: &RolloutHost, target: Option<&HostSummary>, now: DateTime<Utc>) -> Html {
    match units::cluster(h) {
        Some(cluster) => html! {
            <span class="unit-name">
                <RouteLink to={Route::unit(h)} query={crate::components::pipeline::unit_query(h)}>{ units::name(h).to_owned() }</RouteLink>
                <span class="muted">{ format!(" on {cluster}") }</span>
                if h.removal { { " " }<span class="badge" title="It left the configuration; this removes it from the cluster">{ "removing" }</span> }
            </span>
        },
        None => html! { <HostLink id={h.host.clone()} host={target.cloned()} {now} /> },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(hosts: &[(&str, u32)]) -> RolloutPhase {
        serde_json::from_value(serde_json::json!({
            "name": "prod", "state": "awaiting_approval", "parallel": "1", "approval": "required",
            "bake_seconds": 60, "max_failures": "0", "virtual_stage": false,
            "hosts": hosts.iter().map(|(h, changes)| serde_json::json!({
                "host": h, "phase": "prod", "state": "planned", "updated_at": "2026-11-05T00:00:00Z",
                "plan_digest": format!("d-{h}"), "plan_changes": changes, "plan_removals": 0,
                "needs_approval": true,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn plans(p: &RolloutPhase) -> Vec<PlanGroup> {
        p.hosts
            .iter()
            .map(|h| {
                serde_json::from_value(serde_json::json!({
                    "digest": h.plan_digest, "hosts": [h.host], "changes": h.plan_changes,
                    "removals": 0, "destructive": false,
                }))
                .unwrap()
            })
            .collect()
    }

    /// The approval gates the phase, so it binds every plan in it, including
    /// those that change nothing (the server refuses one that leaves any out).
    #[test]
    fn an_approval_binds_unchanged_plans() {
        for hosts in [&[("a", 0), ("b", 0)][..], &[("a", 0), ("b", 2)]] {
            let p = phase(hosts);
            let scope = scope_of(&p, &plans(&p));
            assert_eq!(
                scope.digests.len(),
                hosts.len(),
                "{hosts:?}: {:?}",
                scope.digests
            );
            assert!(scope.without_plan.is_empty(), "{hosts:?}");
        }
    }
}
