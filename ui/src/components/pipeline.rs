//! The rollout pipeline (design §10, §12): stages as columns, every host a
//! rounded block coloured by its rollout state and health, the gate before
//! each stage drawn as a barrier, and the bake countdown on the stage header.
//! Hover or focus a block for the host's name and detail; follow it to open
//! the host. A stage's hosts come first; each cluster's workloads follow
//! in a section under the cluster's name, as round blocks: hollow while a
//! workload waits for an earlier step, a need or its cluster's steward,
//! and saying which.

use std::collections::BTreeMap;

use buckle_api::{HostRolloutState, HostSummary, RolloutDetail, RolloutHost, TargetKind};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::countdown::Countdown;
use super::icons;
use super::link::RouteLink;
use super::meter::{Meter, stage_segments};
use super::ui::Pill;
use crate::app::Route;
use crate::logic::clusters::{Waiting, tally, waiting};
use crate::logic::format::{config_duration, relative, short};
use crate::logic::pipeline::{Gate, GateKind, GateState, OUTSIDE_STAGE, Stage, layout};
use crate::logic::tone::{
    LEGEND, Tone, awaited_reboot, block_look, host_error_headline, host_error_tone,
    host_state_class, host_state_label, outside_block_look, phase_state_label, phase_state_tone,
};
use crate::logic::units;

#[derive(Properties, PartialEq)]
pub struct PipelineProps {
    pub detail: RolloutDetail,
    /// The fleet's hosts, for each block's health and connection.
    #[prop_or_default]
    pub hosts: BTreeMap<String, HostSummary>,
    /// Smaller blocks and no legend, for the Overview.
    #[prop_or_default]
    pub compact: bool,
    /// What each applying host is doing (`nomad › file.template[…]`), for
    /// its block's tooltip.
    #[prop_or_default]
    pub activity: BTreeMap<String, String>,
    pub now: DateTime<Utc>,
}

#[function_component(Pipeline)]
pub fn pipeline(props: &PipelineProps) -> Html {
    let p = layout(&props.detail);
    let units = || props.detail.phases.iter().flat_map(|ph| &ph.hosts);
    let has_cluster = units().any(|h| h.kind != TargetKind::Host);
    let has_removal = units().any(|h| h.removal);
    let stages = p.stages.iter().enumerate().map(|(i, stage)| {
        html! {
            <div class="pipeline__step" role="listitem">
                if stage.phase.name != OUTSIDE_STAGE { { gate(&stage.gate, i == 0, props.now) } }
                { column(stage, props) }
            </div>
        }
    });
    html! {
        <div class={classes!("pipeline", props.compact.then_some("pipeline--compact"))}>
            <div class="pipeline__track" role="list" aria-label="Rollout stages">
                { for stages }
            </div>
            if !props.compact {
                <ul class="legend" aria-label="Block colours">
                    { for LEGEND.iter().map(|s| html! {
                        <li class="legend__item">
                            <span class={classes!("block", "block--legend", host_state_class(*s))} aria-hidden="true" />
                            { host_state_label(*s) }
                        </li>
                    }) }
                    <li class="legend__item">
                        <span class="block block--legend block--healthy block--regressed" aria-hidden="true" />
                        { "health failing" }
                    </li>
                    <li class="legend__item">
                        <span class="block block--legend block--planned block--needs-approval" aria-hidden="true" />
                        { "needs approval" }
                    </li>
                    <li class="legend__item">
                        <span class="block block--legend block--failed block--attention" aria-hidden="true" />
                        { "needs attention" }
                    </li>
                    if has_cluster {
                        <li class="legend__item">
                            <span class="block block--legend block--healthy block--cluster" aria-hidden="true" />
                            { "workload" }
                        </li>
                        <li class="legend__item">
                            <span class="block block--legend block--planned block--cluster block--waiting" aria-hidden="true" />
                            { "waiting (earlier step, need, steward)" }
                        </li>
                    }
                    if has_removal {
                        <li class="legend__item">
                            <span class="block block--legend block--planned block--cluster block--removal" aria-hidden="true" />
                            { "removing" }
                        </li>
                    }
                </ul>
            }
            if !p.duplicates.is_empty() {
                <p class="pipeline__note muted">
                    { format!("Listed in more than one stage and drawn once: {}.", p.duplicates.join(", ")) }
                </p>
            }
        </div>
    }
}

fn gate(gate: &Gate, first: bool, now: DateTime<Utc>) -> Html {
    // Only approvals are drawn as gates. The health check between stages is
    // implicit: it shows only when it holds the rollout (paused), otherwise
    // the stages are just spaced apart.
    if gate.kind == GateKind::Auto && gate.state != GateState::Blocked {
        return if first {
            html! {}
        } else {
            html! { <div class="gate gate--plain" aria-hidden="true" /> }
        };
    }
    let state = match gate.state {
        GateState::Waiting => "waiting",
        GateState::Approved => "approved",
        GateState::AwaitingApproval => "awaiting",
        GateState::Open => "open",
        GateState::Blocked => "blocked",
    };
    let icon = match (gate.kind, gate.state) {
        (_, GateState::Blocked) => icons::pause(),
        (_, GateState::Open) | (_, GateState::Approved) => icons::check(),
        (GateKind::Approval, GateState::Waiting) => icons::clock(),
        (GateKind::Approval, _) => icons::lock(),
        (GateKind::Auto, _) => icons::heart(),
    };
    let mut title = gate.label().to_string();
    if gate.because_destructive {
        title.push_str(" (a plan in this stage is destructive)");
    }
    if let Some(by) = &gate.approved_by {
        title.push_str(&format!(" · approved by {by}"));
    }
    let _ = now;
    html! {
        <div class={classes!("gate", format!("gate--{state}"), (gate.kind == GateKind::Approval).then_some("gate--approval"), gate.closed().then_some("gate--closed"))}
            role="separator" aria-label={title.clone()} title={title}>
            <span class="gate__bar" aria-hidden="true" />
            <span class="gate__icon">{ icon }</span>
            <span class="gate__label">{ gate.label() }</span>
        </div>
    }
}

fn column(stage: &Stage, props: &PipelineProps) -> Html {
    let now = props.now;
    let phase = stage.phase;
    if phase.name == OUTSIDE_STAGE {
        return outside_column(stage, props);
    }
    let c = &stage.counts;
    // "done", not "healthy": the fleet's health is a different reading, and
    // a stage of healthy hosts that has not applied yet is 0 done.
    let mut tally = vec![format!("{}/{} done", c.healthy, c.total)];
    if c.active > 0 {
        tally.push(format!("{} in flight", c.active));
    }
    if c.failed > 0 {
        tally.push(format!("{} failed", c.failed));
    }
    if c.held > 0 {
        tally.push(format!("{} held", c.held));
    }
    if c.plan_pending > 0 {
        tally.push(format!("{} plan pending", c.plan_pending));
    }
    if c.unchanged > 0 {
        tally.push(format!("{} unchanged", c.unchanged));
    }
    // The phase's own deadline, or while it has none, the latest of its
    // baking hosts': the stage is baked when its last host is.
    let until = phase
        .bake_until
        .or_else(|| stage.hosts().filter_map(|h| h.bake_until).max());
    let bake = match until {
        Some(until) if until > now => html! { <Countdown {until} prefix="bake" /> },
        _ if phase.bake_seconds > 0 => {
            html! { <span title="the minimum: a changed module's own bake can be longer, and a host whose run changed nothing does not bake">{ format!("bake {}", config_duration(phase.bake_seconds)) }</span> }
        }
        _ => html! {},
    };
    let style = format!("--per-row: {}", stage.per_row);
    html! {
        <section class={classes!("stage", phase.virtual_stage.then_some("stage--virtual"))}
            data-stage={phase.name.clone()} aria-label={format!("Stage {}", phase.name)}>
            <header class="stage__head">
                <div class="stage__title">
                    <h3 class="stage__name">{ phase.name.clone() }</h3>
                    <Pill tone={phase_state_tone(phase.state)} label={phase_state_label(phase.state)} />
                </div>
                <div class="stage__meta">
                    <span>{ tally.join(" · ") }</span>
                </div>
                <Meter segments={stage_segments(c)} total={c.total} class="stage__meter" />
                <div class="stage__meta muted">
                    if phase.virtual_stage {
                        <span>{ "virtual · opens after promotion" }</span>
                    } else {
                        <span title="hosts applied at a time">{ format!("parallel {}", phase.parallel) }</span>
                        <span title="approval policy">{ phase.approval.replace('_', " ") }</span>
                    }
                    { bake }
                </div>
            </header>
            if stage.counts.total == 0 {
                <p class="stage__empty muted">{ "Nothing to roll" }</p>
            }
            if !stage.targets.is_empty() {
                <div class="stage__blocks" style={style.clone()} role="list">
                    { for stage.targets.iter().map(|h| block(h, props.hosts.get(&h.host), props.activity.get(&h.host), waiting(phase, h), now)) }
                </div>
            }
            { for stage.clusters.iter().map(|c| html! {
                <div class="stage__cluster" data-cluster={c.cluster.to_owned()}>
                    <RouteLink to={Route::Cluster { id: c.cluster.to_owned() }} classes={classes!("stage__cluster-label")}>
                        { c.cluster.to_owned() }
                    </RouteLink>
                    <div class="stage__blocks" style={style.clone()} role="list" aria-label={format!("Workloads on {}", c.cluster)}>
                        { for c.units.iter().map(|h| block(h, props.hosts.get(&h.host), props.activity.get(&units::key(h)), waiting(phase, h), now)) }
                    </div>
                </div>
            }) }
        </section>
    }
}

/// The fleet map's trailing stage: hosts the rollout did not include
/// (registered since, or not selected by any phase), by their own health.
fn outside_column(stage: &Stage, props: &PipelineProps) -> Html {
    let style = format!("--per-row: {}", stage.per_row);
    html! {
        <section class="stage stage--virtual stage--outside" data-stage={OUTSIDE_STAGE} aria-label="Hosts not in this rollout">
            <header class="stage__head">
                <div class="stage__title">
                    <h3 class="stage__name">{ "Not in this rollout" }</h3>
                </div>
                <div class="stage__meta">
                    <span>{ crate::logic::format::count(stage.counts.total, "host", "hosts") }</span>
                </div>
                <div class="stage__meta muted">
                    <span>{ "registered since, or in no phase" }</span>
                </div>
            </header>
            <div class="stage__blocks" {style} role="list">
                { for stage.hosts().map(|h| {
                    let look = outside_block_look(props.hosts.get(&h.host));
                    html! {
                        <RouteLink to={Route::target(&h.host, h.kind)}
                            classes={classes!(look.classes.clone())}
                            label={format!("{}: {}", h.host, look.label)}
                            data_host={h.host.clone()} data_state="outside">
                            <span class="block__tip" aria-hidden="true">
                                <strong class="block__name">{ h.host.clone() }</strong>
                                <span class="block__state">{ look.label.clone() }</span>
                                <span class="muted">{ format!("seen {}", relative(h.updated_at, props.now)) }</span>
                            </span>
                        </RouteLink>
                    }
                }) }
            </div>
        </section>
    }
}

fn block(
    entry: &RolloutHost,
    host: Option<&HostSummary>,
    activity: Option<&String>,
    waits: Option<Waiting>,
    now: DateTime<Utc>,
) -> Html {
    let mut look = block_look(entry, host);
    let activity = activity.filter(|_| entry.state == HostRolloutState::Applying);
    let mut detail: Vec<Html> = Vec::new();
    // A workload names its cluster and who speaks for it; a cluster row how
    // its workloads stand; a unit held for an earlier step (or a steward)
    // says what for.
    if let Some(c) = units::cluster(entry) {
        detail.push(html! { <span>{ "on " }<span class="mono">{ c.to_owned() }</span></span> });
        if let Some(e) = &entry.executor {
            detail.push(html! { <span>{ "via " }<span class="mono">{ e.clone() }</span></span> });
        }
    } else if entry.kind == TargetKind::Cluster {
        if let Some(e) = &entry.executor {
            detail.push(html! { <span>{ "via " }<span class="mono">{ e.clone() }</span></span> });
        }
        if let Some(status) = host.and_then(|h| h.cluster_status.as_ref()) {
            detail.push(html! { <span>{ tally(status).join(" · ") }</span> });
        }
    }
    if let Some(w) = &waits {
        look.classes.push("block--waiting");
        look.label.push_str(&format!(", {}", w.describe()));
        detail.push(html! { <span class="block__note">{ w.describe() }</span> });
    }
    if let Some(now) = activity {
        detail.push(html! { <span class="block__activity">{ "running " }<span class="mono">{ now.clone() }</span></span> });
    }
    if let Some(wave) = entry.wave {
        detail.push(html! { <span>{ format!("wave {wave}") }</span> });
    }
    if entry.removal {
        detail.push(html! { <span>{ "removes the workload: it left the configuration" }</span> });
    }
    if units::unchanged(entry) {
        detail.push(html! { <span class="muted">{ "plan changes nothing" }</span> });
    } else if let Some(digest) = &entry.plan_digest {
        detail.push(html! { <span class="mono">{ format!("plan {}", short(digest)) }</span> });
    }
    if let Some(until) = entry.bake_until.filter(|u| *u > now) {
        let why = match entry.bake.as_ref().and_then(|b| b.module.as_deref()) {
            Some(module) => format!("baking for {module}, "),
            None => "baking, ".to_owned(),
        };
        detail.push(html! { <span>{ why }<Countdown {until} />{ " left" }</span> });
    } else if entry.bake.as_ref().is_some_and(|b| b.unchanged) {
        detail.push(html! { <span class="muted">{ "nothing changed, no bake" }</span> });
    }
    if let Some(reboot) = awaited_reboot(entry, now) {
        let why = match reboot.requested_by.as_slice() {
            [] => String::new(),
            by => format!(" for {}", by.join(", ")),
        };
        let until = reboot.until;
        detail.push(
            html! { <span>{ format!("rebooting{why}, back within ") }<Countdown {until} /></span> },
        );
    }
    if !entry.failed_checks.is_empty() {
        detail
            .push(html! { <span>{ format!("failed: {}", entry.failed_checks.join(", ")) }</span> });
    }
    if !entry.waived_checks.is_empty() {
        detail
            .push(html! { <span>{ format!("waived: {}", entry.waived_checks.join(", ")) }</span> });
    }
    if entry.needs_attention {
        detail.push(html! { <span class="block__error">{ "needs attention: its own rollback failed" }</span> });
    }
    // The headline only: the full error, causes and advice are in the
    // host's row and report.
    if let Some(error) = host_error_headline(entry) {
        let class = if host_error_tone(entry) == Tone::Danger {
            "block__error"
        } else {
            "block__note"
        };
        detail.push(html! { <span class={class}>{ error }</span> });
    }
    detail.push(html! { <span class="muted">{ format!("updated {}", relative(entry.updated_at, now)) }</span> });

    let name = units::name(entry).to_owned();
    html! {
        <RouteLink to={Route::unit(entry)}
            query={unit_query(entry)}
            classes={classes!(look.classes.clone())}
            label={match activity {
                Some(now) => format!("{name}: {}, running {now}", look.label),
                None => format!("{name}: {}", look.label),
            }}
            data_host={entry.host.clone()}
            data_workload={entry.workload.clone()}
            data_state={host_state_label(entry.state).replace(' ', "-")}>
            <span class="block__tip" aria-hidden="true">
                <strong class="block__name">{ name }</strong>
                <span class="block__state">{ look.label.clone() }</span>
                { for detail }
            </span>
        </RouteLink>
    }
}

/// A placement's link opens its workload at the cluster it runs on.
pub(crate) fn unit_query(entry: &RolloutHost) -> Vec<(String, String)> {
    units::cluster(entry)
        .map(|c| vec![("cluster".to_owned(), c.to_owned())])
        .unwrap_or_default()
}
