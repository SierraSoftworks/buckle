//! One distinct plan, as a reviewer reads it (design §11): the hosts that
//! share it, what changes module by module with unified diffs, destructive
//! steps and removals called out, deferred modules with their reason.

use buckle_api::{
    DiffKind, HandlerView, ModuleNote, PlanGroup, PlanView, RehomeView, RemovalView, StepOutcome,
    StepView,
};
use yew::prelude::*;

use super::diff::{DiffView, secret_text};
use super::icons;
use crate::logic::fleet_plan::Tally;
use crate::logic::format::{count, short};
use crate::logic::plan::review;

#[derive(Properties, PartialEq)]
pub struct PlanGroupProps {
    pub group: PlanGroup,
    /// The hosts of the phase under review; others sharing the plan are
    /// listed apart.
    #[prop_or_default]
    pub focus: Vec<String>,
    #[prop_or_default]
    pub open: bool,
}

#[function_component(PlanGroupView)]
pub fn plan_group(props: &PlanGroupProps) -> Html {
    let g = &props.group;
    let r = g.plan.as_ref().map(review);
    let (here, elsewhere): (Vec<&String>, Vec<&String>) = if props.focus.is_empty() {
        (g.hosts.iter().collect(), Vec::new())
    } else {
        g.hosts.iter().partition(|h| props.focus.contains(h))
    };
    let destructive = r
        .as_ref()
        .map(|r| r.destructive())
        .unwrap_or(g.removals as usize);
    let summary = html! {
        <summary class="plan__summary">
            <span class="plan__digest mono" title={g.digest.clone()}>{ short(&g.digest) }</span>
            <span class="plan__hosts">{ here.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ") }</span>
            <span class="plan__counts">
                { count(g.changes as usize, "change", "changes") }
                if g.removals > 0 { { " · " }{ count(g.removals as usize, "removal", "removals") } }
            </span>
            if g.destructive {
                <span class="badge badge--danger">{ icons::alert() }{ format!("{destructive} destructive") }</span>
            }
        </summary>
    };
    html! {
        <details class={classes!("plan", g.destructive.then_some("plan--destructive"))} open={props.open}
            data-digest={g.digest.clone()}>
            { summary }
            <div class="plan__body">
                if !elsewhere.is_empty() {
                    <p class="muted">{ format!("Also the plan of {} in other stages.", elsewhere.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")) }</p>
                }
                { match &g.plan {
                    Some(plan) => body(plan),
                    None => html! { <p class="muted">{ "The plan document is not available; the counts above come from the server's summary." }</p> },
                } }
            </div>
        </details>
    }
}

/// What a plan line does, drawn as a coloured icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Create,
    Modify,
    Remove,
    /// A removal that only forgets the record.
    Forget,
    Blocked,
    Unchanged,
}

impl Mark {
    pub fn of(s: &StepView) -> Mark {
        match (s.outcome, s.diff) {
            (StepOutcome::NoOp, _) => Mark::Unchanged,
            (StepOutcome::Blocked, _) => Mark::Blocked,
            (_, Some(DiffKind::Create)) => Mark::Create,
            (_, Some(DiffKind::Delete)) => Mark::Remove,
            _ => Mark::Modify,
        }
    }

    fn parts(self) -> (&'static str, &'static str, Html) {
        match self {
            Mark::Create => ("create", "creates", icons::create()),
            Mark::Modify => ("modify", "modifies", icons::modify()),
            Mark::Remove => ("remove", "removes", icons::remove()),
            Mark::Forget => ("forget", "forgets", icons::remove()),
            Mark::Blocked => ("blocked", "blocked", icons::blocked()),
            Mark::Unchanged => ("unchanged", "unchanged", icons::check()),
        }
    }
}

/// A line's coloured change icon.
pub fn mark(m: Mark) -> Html {
    let (class, label, icon) = m.parts();
    html! {
        <span class={classes!("mark", format!("mark--{class}"))} title={label}>
            { icon }<span class="visually-hidden">{ format!("{label} ") }</span>
        </span>
    }
}

/// A collapsible block of a plan (a module, removals, notes), closed to its
/// heading line until opened, its lines indented under it.
pub fn block(class: &'static str, head: Html, body: Html) -> Html {
    html! {
        <details class={classes!("plan-module", class)}>
            <summary class="plan-module__head">
                <span class="plan-module__chevron">{ icons::chevron() }</span>
                { head }
            </summary>
            <div class="plan-module__body">{ body }</div>
        </details>
    }
}

/// A section of modules the plan only has a note about.
fn notes(
    title: &str,
    aside: &str,
    glyph: &str,
    class: &str,
    list: &[ModuleNote],
    footer: Html,
) -> Html {
    if list.is_empty() {
        return html! {};
    }
    block(
        "",
        notes_head(title, aside, list.len()),
        html! { <>
            <ul class="steps">
                { for list.iter().map(|n| note(n, glyph, class, html! {})) }
            </ul>
            { footer }
        </> },
    )
}

/// The heading of a section of module notes.
pub fn notes_head(title: &str, aside: &str, modules: usize) -> Html {
    html! { <>
        <h4 class="plan-module__name">{ title.to_string() }</h4>
        if !aside.is_empty() { <span class="muted">{ aside.to_string() }</span> }
        <span class="plan-module__aside muted">{ count(modules, "module", "modules") }</span>
    </> }
}

/// One deferred, held or indeterminate module.
pub fn note(n: &ModuleNote, glyph: &str, class: &str, scope: Html) -> Html {
    html! {
        <li class={classes!("step", class.to_string())}>
            <span class="step__glyph" aria-hidden="true">{ glyph.to_string() }</span>
            <span class="mono">{ n.module.clone() }</span>
            <span class="step__summary">{ secret_text(&n.reason) }</span>
            if !n.waiting_on.is_empty() {
                <span class="muted small">{ format!("waits for {}", n.waiting_on.join(", ")) }</span>
            }
            if n.plan_error { <span class="badge badge--danger">{ "plan error" }</span> }
            { scope }
        </li>
    }
}

fn body(plan: &PlanView) -> Html {
    let warnings = (!plan.warnings.is_empty()).then(|| html! {
        <ul class="plan__warnings">
            { for plan.warnings.iter().map(|w| html! { <li>{ icons::alert() }{ secret_text(w) }</li> }) }
        </ul>
    });
    // A module with nothing to apply is left out; the rest open to their
    // diffs from a heading that counts them.
    let modules = plan.modules.iter().filter_map(|m| {
        let changed: Vec<&StepView> = m
            .steps
            .iter()
            .filter(|s| s.outcome != StepOutcome::NoOp)
            .collect();
        if changed.is_empty() && m.handlers.is_empty() {
            return None;
        }
        Some(block(
            "",
            module_head(
                &m.module,
                &m.needs,
                Tally::of_steps(changed.iter().copied()),
                html! {},
            ),
            html! {
                <ul class="steps">
                    { for changed.into_iter().map(|s| step(s, html! {})) }
                    { for m.handlers.iter().map(|h| handler(&m.module, h, html! {})) }
                </ul>
            },
        ))
    });
    let removals = (!plan.removals.is_empty()).then(|| {
        removals_block(
            plan.removals.len(),
            html! {},
            html! { { for plan.removals.iter().map(|r| removal(r, html! {})) } },
        )
    });
    let rehomed = (!plan.rehomed.is_empty()).then(|| {
        rehomed_block(
            plan.rehomed.len(),
            html! {},
            html! { { for plan.rehomed.iter().map(|r| rehome(r, html! {})) } },
        )
    });
    html! { <>
        { warnings.unwrap_or_default() }
        { for modules }
        { removals.unwrap_or_default() }
        { rehomed.unwrap_or_default() }
        { notes("Deferred", "", "…", "step--deferred", &plan.deferred,
            html! { <p class="muted small">{ DEFERRED_NOTE }</p> }) }
        { notes("Indeterminate", INDETERMINATE_NOTE, "?", "step--blocked", &plan.indeterminate, html! {}) }
        { notes("Held", HELD_NOTE, "‖", "step--deferred", &plan.held, html! {}) }
        if !plan.skipped.is_empty() {
            <p class="muted small">{ format!("Not selected: {}", plan.skipped.join("; ")) }</p>
        }
    </> }
}

pub const DEFERRED_NOTE: &str = "If a deferred module's plan turns out destructive, the agent stops before it and asks for approval again.";
pub const INDETERMINATE_NOTE: &str = "not applied; their resources are never removed";
pub const HELD_NOTE: &str = "not applied at this revision; left alone";

/// A module's heading: its name and what it needs, then (right-aligned)
/// what it changes and, in a fleet-wide plan, on how many hosts.
pub fn module_head(module: &str, needs: &[String], changes: Tally, hosts: Html) -> Html {
    html! { <>
        <h4 class="plan-module__name mono">{ module.to_string() }</h4>
        if !needs.is_empty() { <span class="muted">{ format!("needs: {}", needs.join(", ")) }</span> }
        <span class="plan-module__aside">{ tally(changes) }{ hosts }</span>
    </> }
}

/// `+2 ~1 -1`: what a block creates, modifies and removes (and is blocked
/// on), each in its change colour; kinds it has none of are left out.
pub fn tally(t: Tally) -> Html {
    let part = |n: usize, class: &'static str, sign: &'static str, what: &'static str| {
        (n > 0).then(|| html! {
            <span class={classes!("tally__part", format!("tally__part--{class}"))} title={format!("{n} {what}")}>
                { format!("{sign}{n}") }
            </span>
        })
    };
    html! {
        <span class="tally" data-testid="tally">
            { part(t.create, "create", "+", "to create").unwrap_or_default() }
            { part(t.modify, "modify", "~", "to modify").unwrap_or_default() }
            { part(t.remove, "remove", "-", "to remove").unwrap_or_default() }
            { part(t.blocked, "blocked", "!", "blocked").unwrap_or_default() }
        </span>
    }
}

pub fn removals_block(n: usize, hosts: Html, items: Html) -> Html {
    block(
        "plan-module--removals",
        html! { <>
            <h4 class="plan-module__name">{ "Removals" }</h4>
            <span class="muted">{ "resources the revision no longer declares" }</span>
            <span class="plan-module__aside">{ tally(Tally { remove: n, ..Tally::default() }) }{ hosts }</span>
        </> },
        html! { <ul class="steps">{ items }</ul> },
    )
}

pub fn rehomed_block(n: usize, hosts: Html, items: Html) -> Html {
    block(
        "",
        html! { <>
            <h4 class="plan-module__name">{ "Taken over" }</h4>
            <span class="muted">{ "records a declared resource now manages, kept instead of removed" }</span>
            <span class="plan-module__aside"><span class="muted">{ count(n, "record", "records") }</span>{ hosts }</span>
        </> },
        html! { <ul class="steps">{ items }</ul> },
    )
}

pub fn handler(module: &str, h: &HandlerView, scope: Html) -> Html {
    html! {
        <li class="step step--handler">
            <span class="mark mark--handler" title="handler">{ icons::refresh() }</span>
            <span class="mono">{ format!("handler {module}:{}", h.handler) }</span>
            <span class="step__summary">{ secret_text(&h.summary) }</span>
            { scope }
        </li>
    }
}

pub fn removal(r: &RemovalView, scope: Html) -> Html {
    html! {
        <li class={classes!("step", if r.cleanup { "step--destructive" } else { "step--forget" })}>
            { mark(if r.cleanup { Mark::Remove } else { Mark::Forget }) }
            <span class="mono step__resource">{ secret_text(&r.resource()) }</span>
            <span class="step__summary">{ secret_text(&r.summary) }</span>
            if r.cleanup { <span class="badge badge--danger">{ "removal" }</span> }
            else { <span class="badge">{ "forget only" }</span> }
            if r.caution && r.cleanup {
                <span class="badge badge--warn" title="This kind's removal can take unrelated things down (dependent packages, files owned by a user).">{ icons::alert() }{ "caution" }</span>
            }
            { scope }
        </li>
    }
}

pub fn rehome(r: &RehomeView, scope: Html) -> Html {
    html! {
        <li class="step step--forget">
            <span class="step__glyph" aria-hidden="true">{ "→" }</span>
            <span class="mono step__resource">{ secret_text(&format!("{} → {}", r.from, r.to)) }</span>
            <span class="step__summary">{ secret_text(&r.summary) }</span>
            <span class="badge">{ r.how.clone() }</span>
            { scope }
        </li>
    }
}

/// A step's diff or preview, indented under it.
pub fn step_detail(s: &StepView) -> Html {
    if s.outcome != StepOutcome::Change {
        return html! {};
    }
    match (&s.unified, &s.preview) {
        (Some(u), _) => html! { <div class="step__detail"><DiffView unified={u.clone()} /></div> },
        (None, Some(p)) => {
            html! { <div class="step__detail"><pre class="step__preview">{ secret_text(p) }</pre></div> }
        }
        _ => html! {},
    }
}

/// A step's heading line, without its detail.
pub fn step_line(s: &StepView, extra: Html) -> Html {
    let summary = match s.outcome {
        StepOutcome::Change => s.summary.clone(),
        StepOutcome::Blocked => format!("blocked: {}", s.summary),
        StepOutcome::NoOp => "no change".into(),
    };
    html! { <>
        { mark(Mark::of(s)) }
        <span class="mono step__resource">{ secret_text(&s.resource()) }</span>
        <span class="step__summary">{ secret_text(&summary) }</span>
        if s.destructive { <span class="badge badge--danger">{ icons::alert() }{ "destructive" }</span> }
        if !s.locks.is_empty() { <span class="badge" title="resource locks">{ format!("lock: {}", s.locks.join(", ")) }</span> }
        if !s.notifies.is_empty() { <span class="muted small">{ format!("→ notify {}", s.notifies.join(", ")) }</span> }
        if s.always_runs { <span class="badge badge--warn">{ "always runs" }</span> }
        { extra }
    </> }
}

pub fn step_class(s: &StepView) -> Classes {
    classes!(
        "step",
        s.destructive.then_some("step--destructive"),
        (s.outcome == StepOutcome::Blocked).then_some("step--blocked")
    )
}

fn step(s: &StepView, scope: Html) -> Html {
    html! {
        <li class={step_class(s)}>
            { step_line(s, scope) }
            { step_detail(s) }
        </li>
    }
}
