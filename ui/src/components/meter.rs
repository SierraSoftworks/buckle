//! A segmented bar: how a whole (a rollout's hosts, a stage, the fleet)
//! splits into states, readable at a glance; hover it for the numbers.

use buckle_api::RolloutSummary;
use yew::prelude::*;

use crate::logic::pipeline::StageCounts;
use crate::logic::tone::Tone;

/// One coloured part of a [`Meter`].
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub count: usize,
    pub tone: Tone,
    pub label: &'static str,
}

impl Segment {
    pub fn new(count: usize, tone: Tone, label: &'static str) -> Self {
        Segment { count, tone, label }
    }
}

#[derive(Properties, PartialEq)]
pub struct MeterProps {
    pub segments: Vec<Segment>,
    /// The whole; what the segments leave of it is drawn as the empty track.
    pub total: usize,
    /// What the remainder is called in the title (`to go`).
    #[prop_or("to go")]
    pub rest: &'static str,
    #[prop_or_default]
    pub class: Classes,
}

#[function_component(Meter)]
pub fn meter(props: &MeterProps) -> Html {
    let shown: Vec<&Segment> = props.segments.iter().filter(|s| s.count > 0).collect();
    let used: usize = shown.iter().map(|s| s.count).sum();
    let rest = props.total.saturating_sub(used);
    let mut summary: Vec<String> = shown
        .iter()
        .map(|s| format!("{} {}", s.count, s.label))
        .collect();
    if rest > 0 {
        summary.push(format!("{rest} {}", props.rest));
    }
    let summary = summary.join(" · ");
    html! {
        <span class={classes!("meter", props.class.clone())} role="img" aria-label={summary.clone()} title={summary}>
            { for shown.into_iter().map(|s| html! {
                <span class={classes!("meter__seg", s.tone.class())} style={format!("flex-grow: {}", s.count)} />
            }) }
            if rest > 0 { <span class="meter__seg meter__seg--rest" style={format!("flex-grow: {rest}")} /> }
        </span>
    }
}

/// A rollout's hosts: done, failed and still to go.
pub fn rollout_segments(s: &RolloutSummary) -> Vec<Segment> {
    vec![
        Segment::new(s.hosts_done as usize, Tone::Ok, "done"),
        Segment::new(s.hosts_failed as usize, Tone::Danger, "failed"),
    ]
}

/// A stage's hosts by where they stand in the rollout.
pub fn stage_segments(c: &StageCounts) -> Vec<Segment> {
    vec![
        Segment::new(c.healthy, Tone::Ok, "done"),
        Segment::new(c.active, Tone::Active, "in flight"),
        Segment::new(c.held, Tone::Warn, "held"),
        Segment::new(c.failed, Tone::Danger, "failed"),
        Segment::new(c.rolled_back, Tone::Mute, "rolled back"),
        Segment::new(c.skipped, Tone::Neutral, "skipped"),
    ]
}
