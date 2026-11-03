//! The rollout pipeline (design §10, §12): stages as columns, every host a
//! rounded block coloured by its rollout state and health, the gate before
//! each stage drawn as a barrier, and the bake countdown on the stage header.
//! Hover or focus a block for the host's name and detail; follow it to open
//! the host.

use std::collections::BTreeMap;

use buckle_api::{HostRolloutState, HostSummary, RolloutDetail, RolloutHost, RolloutPhase};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::countdown::Countdown;
use super::icons;
use super::link::RouteLink;
use super::meter::{Meter, stage_segments};
use super::ui::Pill;
use crate::app::Route;
use crate::logic::format::{config_duration, relative, short};
use crate::logic::pipeline::{
    Gate, GateKind, GateState, OUTSIDE_STAGE, Stage, gate_check_tone, layout,
};
use crate::logic::tone::{
    LEGEND, Tone, block_look, host_error_headline, host_error_tone, host_state_class,
    host_state_label, outside_block_look, phase_state_label, phase_state_tone,
};

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
    // Before the first stage there is nothing to pass unless it needs approval.
    if first && gate.kind == GateKind::Auto {
        return html! {};
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

/// A phase's health gate (design §12 mock): one chip per required check of
/// the modules its hosts apply, red where a host fails it.
pub fn health_gate(phase: &RolloutPhase) -> Html {
    if phase.health_checks.is_empty() {
        return html! {};
    }
    html! {
        <ul class="gate-checks" aria-label={format!("Health gate of {}", phase.name)}>
            { for phase.health_checks.iter().map(|c| html! {
                <li><Pill tone={gate_check_tone(phase, c)} label={c.clone()} /></li>
            }) }
        </ul>
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
    // The phase's own deadline, or while it has none, the latest of its
    // baking hosts': the stage is baked when its last host is.
    let until = phase
        .bake_until
        .or_else(|| stage.hosts().filter_map(|h| h.bake_until).max());
    let bake = match until {
        Some(until) if until > now => html! { <Countdown {until} prefix="bake" /> },
        _ if phase.bake_seconds > 0 => {
            html! { <span>{ format!("bake {}", config_duration(phase.bake_seconds)) }</span> }
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
                <p class="stage__empty muted">{ "No hosts" }</p>
            } else {
                <div class="stage__blocks" {style} role="list">
                    { for stage.hosts().map(|h| block(h, props.hosts.get(&h.host), props.activity.get(&h.host), now)) }
                </div>
            }
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
                        <RouteLink to={Route::Host { id: h.host.clone() }}
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
    now: DateTime<Utc>,
) -> Html {
    let look = block_look(entry, host);
    let activity = activity.filter(|_| entry.state == HostRolloutState::Applying);
    let mut detail: Vec<Html> = Vec::new();
    if let Some(now) = activity {
        detail.push(html! { <span class="block__activity">{ "running " }<span class="mono">{ now.clone() }</span></span> });
    }
    if let Some(wave) = entry.wave {
        detail.push(html! { <span>{ format!("wave {wave}") }</span> });
    }
    if let Some(digest) = &entry.plan_digest {
        detail.push(html! { <span class="mono">{ format!("plan {}", short(digest)) }</span> });
    }
    if let Some(until) = entry.bake_until.filter(|u| *u > now) {
        detail.push(html! { <span>{ "baking, " }<Countdown {until} />{ " left" }</span> });
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

    html! {
        <RouteLink to={Route::Host { id: entry.host.clone() }}
            classes={classes!(look.classes.clone())}
            label={match activity {
                Some(now) => format!("{}: {}, running {now}", entry.host, look.label),
                None => format!("{}: {}", entry.host, look.label),
            }}
            data_host={entry.host.clone()}
            data_state={host_state_label(entry.state).replace(' ', "-")}>
            <span class="block__tip" aria-hidden="true">
                <strong class="block__name">{ entry.host.clone() }</strong>
                <span class="block__state">{ look.label.clone() }</span>
                { for detail }
            </span>
        </RouteLink>
    }
}
