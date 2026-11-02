//! The rollout pipeline (design §10, §12): stages as columns, every host a
//! rounded block coloured by its rollout state and health, the gate before
//! each stage drawn as a barrier, and the bake countdown on the stage header.
//! Hover or focus a block for the host's name and detail; follow it to open
//! the host.

use std::collections::BTreeMap;

use buckle_api::{HostSummary, RolloutDetail, RolloutHost, RolloutPhase};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::countdown::Countdown;
use super::icons;
use super::link::RouteLink;
use super::ui::Pill;
use crate::app::Route;
use crate::logic::format::{config_duration, relative, short};
use crate::logic::pipeline::{Gate, GateKind, GateState, Stage, gate_check_tone, layout};
use crate::logic::tone::{
    LEGEND, block_look, host_state_class, host_state_label, phase_state_label, phase_state_tone,
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
    pub now: DateTime<Utc>,
}

#[function_component(Pipeline)]
pub fn pipeline(props: &PipelineProps) -> Html {
    let p = layout(&props.detail);
    let stages = p.stages.iter().enumerate().map(|(i, stage)| {
        html! {
            <div class="pipeline__step" role="listitem">
                { gate(&stage.gate, i == 0, props.now) }
                { column(stage, &props.hosts, props.compact, props.now) }
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

fn column(
    stage: &Stage,
    hosts: &BTreeMap<String, HostSummary>,
    compact: bool,
    now: DateTime<Utc>,
) -> Html {
    let phase = stage.phase;
    let c = &stage.counts;
    let mut tally = vec![format!("{}/{} healthy", c.healthy, c.total)];
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
                <div class="stage__meta muted">
                    if phase.virtual_stage {
                        <span>{ "virtual · opens after promotion" }</span>
                    } else {
                        <span title="hosts applied at a time">{ format!("parallel {}", phase.parallel) }</span>
                        <span title="approval policy">{ phase.approval.replace('_', " ") }</span>
                    }
                    { bake }
                </div>
                if !compact { { health_gate(phase) } }
            </header>
            if stage.counts.total == 0 {
                <p class="stage__empty muted">{ "No hosts" }</p>
            } else {
                <div class="stage__blocks" {style} role="list">
                    { for stage.hosts().map(|h| block(h, hosts.get(&h.host), now)) }
                </div>
            }
        </section>
    }
}

fn block(entry: &RolloutHost, host: Option<&HostSummary>, now: DateTime<Utc>) -> Html {
    let look = block_look(entry, host);
    let mut detail: Vec<Html> = Vec::new();
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
    if let Some(error) = &entry.error {
        detail.push(html! { <span class="block__error">{ error.clone() }</span> });
    }
    detail.push(html! { <span class="muted">{ format!("updated {}", relative(entry.updated_at, now)) }</span> });

    html! {
        <RouteLink to={Route::Host { id: entry.host.clone() }}
            classes={classes!(look.classes.clone())}
            label={format!("{}: {}", entry.host, look.label)}
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
