//! The two pills every aggregate shares: what a rollout, host or module
//! changes (`+2 | ~1 | -0`), and how long something took (a stopwatch).

use yew::prelude::*;

use super::icons;
use crate::logic::fleet_plan::Tally;

#[derive(Properties, PartialEq)]
pub struct ChangesProps {
    pub tally: Tally,
}

/// One pill in three parts, creations, modifications and removals, each in
/// its change colour (a zero part dimmed), and a fourth for blocked steps
/// when there are any.
#[function_component(Changes)]
pub fn changes(props: &ChangesProps) -> Html {
    let t = props.tally;
    let mut parts = vec![
        (t.create, "create", "+", "to create"),
        (t.modify, "modify", "~", "to modify"),
        (t.remove, "remove", "-", "to remove"),
    ];
    if t.blocked > 0 {
        parts.push((t.blocked, "blocked", "!", "blocked"));
    }
    let title = parts
        .iter()
        .map(|(n, _, _, what)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(", ");
    html! {
        <span class="change-pill" data-testid="tally" {title}>
            { for parts.into_iter().map(|(n, class, sign, _)| html! {
                <span class={classes!("change-pill__part", format!("change-pill__part--{class}"), (n == 0).then_some("change-pill__part--zero"))}
                    data-part={class}>
                    { format!("{sign}{n}") }
                </span>
            }) }
        </span>
    }
}

#[derive(Properties, PartialEq)]
pub struct RunChangesProps {
    /// Steps that changed something (created or modified).
    pub changes: u32,
    pub removals: u32,
}

/// A run's coarse counts in the same pill: a run summary says how many steps
/// changed something and how many removals ran, not which kind of change.
#[function_component(RunChanges)]
pub fn run_changes(props: &RunChangesProps) -> Html {
    let (c, r) = (props.changes, props.removals);
    let part = |n: u32, class: &str, sign: &str| {
        html! {
            <span class={classes!("change-pill__part", format!("change-pill__part--{class}"), (n == 0).then_some("change-pill__part--zero"))}
                data-part={class.to_string()}>
                { format!("{sign}{n}") }
            </span>
        }
    };
    html! {
        <span class="change-pill" data-testid="tally" title={format!("{c} changed (created or modified), {r} removed")}>
            { part(c, "modify", "~") }
            { part(r, "remove", "-") }
        </span>
    }
}

#[derive(Properties, PartialEq)]
pub struct TookProps {
    /// The formatted duration (`2m 05s`).
    pub label: AttrValue,
    /// Still running: the time so far.
    #[prop_or_default]
    pub running: bool,
}

/// How long something took (or has taken so far), behind a stopwatch.
#[function_component(Took)]
pub fn took(props: &TookProps) -> Html {
    let title = if props.running {
        format!("running for {}", props.label)
    } else {
        format!("took {}", props.label)
    };
    html! {
        <span class={classes!("took", props.running.then_some("took--running"))} title={title}>
            { icons::stopwatch() }{ props.label.clone() }
        </span>
    }
}
