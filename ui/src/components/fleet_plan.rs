//! A rollout's plan as one union across its hosts and phases: each change
//! marked as fleet-wide or naming the phases or hosts it applies to, host-specific values shown
//! as fact placeholders (with sample values on hover), and a tab per version
//! where a change still differs between hosts.

use std::rc::Rc;

use buckle_api::{FleetPlan, FleetStep, PlanGroup, Shared};
use yew::prelude::*;

use crate::components::ui::{Note, NoteKind};

use super::diff::{FactExemplars, secret_text};
use super::icons;
use super::plan::{
    DEFERRED_NOTE, HELD_NOTE, INDETERMINATE_NOTE, block, handler, module_head, note, notes_head,
    rehome, rehomed_block, removal, removals_block, step_class, step_detail, step_line,
};
use crate::logic::fleet_plan::{
    Phases, hosts_changed, phase_label, scope_label, scope_title, summarise,
};
use crate::logic::format::count;

#[derive(Properties, PartialEq)]
pub struct FleetPlanProps {
    pub plan: FleetPlan,
}

/// Which hosts a line applies to: all of them, or a named subset (by
/// phase, across phases).
fn scope(all: &[String], hosts: &[String], phases: &Phases) -> Html {
    let (fleet_wide, label) = scope_label(all, hosts, phases);
    html! {
        <span class={classes!("scope", if fleet_wide { "scope--all" } else { "scope--subset" })}
            title={scope_title(hosts, phases)}>
            { label }
        </span>
    }
}

fn shared_scope<T>(all: &[String], s: &Shared<T>, phases: &Phases) -> Html {
    scope(all, &s.hosts, phases)
}

/// How many of the hosts under review a block changes, for its heading;
/// across phases, the phases it changes where they say it.
fn changed_of(all: &[String], changed: &[String], phases: &Phases) -> Html {
    let fleet_wide = all.iter().all(|h| changed.contains(h));
    html! {
        <span class={classes!("scope", if fleet_wide { "scope--all" } else { "scope--subset" })}
            title={scope_title(changed, phases)} data-testid="hosts-changed">
            { (!fleet_wide).then(|| phase_label(all, changed, phases)).flatten()
                .map(|p| format!("{p} changed"))
                .unwrap_or_else(|| hosts_changed(all.len(), changed.len())) }
        </span>
    }
}

/// The hosts any of `list` applies to.
fn union<T>(list: &[Shared<T>]) -> Vec<String> {
    let set: std::collections::BTreeSet<&String> = list.iter().flat_map(|s| &s.hosts).collect();
    set.into_iter().cloned().collect()
}

/// A line's scope, unless it is the same as its module's.
fn scope_within(all: &[String], module: &[String], hosts: &[String], phases: &Phases) -> Html {
    if module.iter().all(|h| hosts.contains(h)) {
        html! {}
    } else {
        scope(all, hosts, phases)
    }
}

#[function_component(FleetPlanView)]
pub fn fleet_plan_view(props: &FleetPlanProps) -> Html {
    let p = &props.plan;
    let all = &p.hosts;
    let phases = use_context::<Phases>().unwrap_or_default();
    let ph = &phases;
    let exemplars = use_memo(p.facts.clone(), |f| {
        FactExemplars(Rc::new(FleetPlan {
            facts: f.clone(),
            ..Default::default()
        }))
    });
    // Modules that change nothing anywhere are left out; the rest are
    // headed by what they change on how many hosts, a line within naming
    // its own hosts only where it changes fewer than its module does.
    let modules = p.modules.iter().filter_map(|m| {
        let summary = summarise(m);
        if !summary.has_changes() {
            return None;
        }
        let changed: Vec<&FleetStep> = m.steps.iter().filter(|s| !s.variants.is_empty()).collect();
        let within = summary.changed.clone();
        Some(block(
            "",
            module_head(&m.module, &m.needs, !m.deferred.is_empty(), summary.tally, changed_of(all, &summary.changed, ph)),
            html! {
                <ul class="steps">
                    { for changed.into_iter().map(|s| html! {
                        <FleetStepView step={s.clone()} all={all.clone()} module={within.clone()} />
                    }) }
                    { for m.handlers.iter().map(|h| handler(&m.module, &h.value, scope_within(all, &within, &h.hosts, ph))) }
                </ul>
            },
        ))
    });
    let removals = (!p.removals.is_empty()).then(|| {
        removals_block(
            p.removals.len(),
            changed_of(all, &union(&p.removals), ph),
            html! { { for p.removals.iter().map(|r| removal(&r.value, shared_scope(all, r, ph))) } },
        )
    });
    let rehomed = (!p.rehomed.is_empty()).then(|| {
        rehomed_block(
            p.rehomed.len(),
            changed_of(all, &union(&p.rehomed), ph),
            html! { { for p.rehomed.iter().map(|r| rehome(&r.value, shared_scope(all, r, ph))) } },
        )
    });
    let notes = |title: &str,
                 aside: &str,
                 glyph: &str,
                 class: &str,
                 list: &[Shared<buckle_api::ModuleNote>],
                 footer: Html| {
        if list.is_empty() {
            return html! {};
        }
        block(
            "",
            notes_head(title, aside, list.len()),
            html! { <>
                <ul class="steps">{ for list.iter().map(|n| note(&n.value, glyph, class, shared_scope(all, n, ph))) }</ul>
                { footer }
            </> },
        )
    };
    html! {
        <ContextProvider<FactExemplars> context={(*exemplars).clone()}>
            <div class="fleet-plan" data-testid="fleet-plan">
                <p class="fleet-plan__intro muted small">
                    { format!("The modules that change something on {}{}, each with what it changes on how many; open one for its diffs. ",
                        count(all.len(), "host", "hosts"),
                        match ph.0.iter().filter(|(_, h)| h.iter().any(|x| all.contains(x))).count() {
                            0 | 1 => String::new(),
                            n => format!(" in {n} phases"),
                        }) }
                    if !p.facts.is_empty() {
                        { " " }<span class="fact fact--legend">{ icons::host() }<span class="fact__path">{ "fact" }</span></span>
                        { " stands for a value that differs per host; hover it for examples." }
                    }
                </p>
                if !p.unplanned.is_empty() {
                    <Note kind={NoteKind::Warn}>{ format!("Not included, no plan of this revision yet: {}.", p.unplanned.join(", ")) }</Note>
                }
                { for modules }
                { removals.unwrap_or_default() }
                { rehomed.unwrap_or_default() }
                { notes("Deferred", "", "…", "step--deferred", &p.deferred, html! { <p class="muted small">{ DEFERRED_NOTE }</p> }) }
                { notes("Indeterminate", INDETERMINATE_NOTE, "?", "step--blocked", &p.indeterminate, html! {}) }
                { notes("Held", HELD_NOTE, "‖", "step--deferred", &p.held, html! {}) }
                if !p.skipped.is_empty() {
                    <p class="muted small">{ format!("Not selected: {}", p.skipped.iter().map(|s| s.value.as_str()).collect::<Vec<_>>().join("; ")) }</p>
                }
            </div>
        </ContextProvider<FactExemplars>>
    }
}

#[derive(Properties, PartialEq)]
struct FleetStepProps {
    step: FleetStep,
    all: Vec<String>,
    /// The hosts its module changes.
    module: Vec<String>,
}

/// One resource's change across hosts, with a tab per version where it differs.
#[function_component(FleetStepView)]
fn fleet_step_view(props: &FleetStepProps) -> Html {
    let s = &props.step;
    let phases = use_context::<Phases>().unwrap_or_default();
    let variant = use_state(|| 0usize);
    let index = (*variant).min(s.variants.len().saturating_sub(1));
    let Some(chosen) = s.variants.get(index) else {
        return html! {};
    };
    let changed: Vec<String> = s.changed_hosts().cloned().collect();
    let extra = html! { <>
        { scope_within(&props.all, &props.module, &changed, &phases) }
        if s.variants.len() > 1 {
            <span class="badge badge--info" title="The change differs between hosts beyond their facts.">
                { format!("varies: {} versions", s.variants.len()) }
            </span>
        }
    </> };
    let tabs = (s.variants.len() > 1).then(|| html! {
        <div class="tabs tabs--small step__variants" role="tablist" aria-label="Versions of this change">
            { for s.variants.iter().enumerate().map(|(i, v)| {
                let variant = variant.clone();
                html! {
                    <button type="button" role="tab" aria-selected={(i == index).to_string()}
                        class={classes!("tabs__tab", (i == index).then_some("tabs__tab--active"))}
                        title={v.hosts.join(", ")}
                        onclick={Callback::from(move |_| variant.set(i))}>
                        { format!("Version {} · {}", i + 1, count(v.hosts.len(), "host", "hosts")) }
                    </button>
                }
            }) }
        </div>
    });
    html! {
        <li class={step_class(&chosen.value)} data-resource={s.resource()}>
            { step_line(&chosen.value, extra) }
            { tabs.unwrap_or_default() }
            { step_detail(&chosen.value) }
        </li>
    }
}

/// The plan's warnings, each a banner beside the rollout's other notes,
/// naming its phases or hosts where it does not concern every host.
#[function_component(FleetWarnings)]
fn fleet_warnings(props: &FleetPlanProps) -> Html {
    let p = &props.plan;
    let phases = use_context::<Phases>().unwrap_or_default();
    let exemplars = use_memo(p.facts.clone(), |f| {
        FactExemplars(Rc::new(FleetPlan {
            facts: f.clone(),
            ..Default::default()
        }))
    });
    if p.warnings.is_empty() {
        return html! {};
    }
    html! {
        <ContextProvider<FactExemplars> context={(*exemplars).clone()}>
        { for p.warnings.iter().map(|w| html! {
            <Note kind={NoteKind::Warn} testid="plan-warning"
                aside={scope_within(&p.hosts, &p.hosts, &w.hosts, &phases)}>
                { secret_text(&w.value) }
            </Note>
        }) }
        </ContextProvider<FactExemplars>>
    }
}

#[derive(Properties, PartialEq)]
pub struct PlanViewsProps {
    /// The union of the hosts' plans, when the server sends one.
    pub fleet: Option<FleetPlan>,
    /// The distinct plans, for the per-plan view and hosts' own renders.
    pub groups: Vec<PlanGroup>,
    /// The rollout's phases and their hosts, so a line names the phases it
    /// covers.
    #[prop_or_default]
    pub phases: Phases,
    /// Open any destructive plan in the per-plan view.
    #[prop_or_default]
    pub open_destructive: bool,
}

/// A rollout's plans across all its phases, as one fleet-wide union (the
/// default) or as the distinct plans that approval binds.
#[function_component(PlanViews)]
pub fn plan_views(props: &PlanViewsProps) -> Html {
    let by_plan = use_state(|| false);
    let by_plan_view = html! {
        <div class="plans">
            { for props.groups.iter().map(|g| html! {
                <super::PlanGroupView group={g.clone()} open={props.open_destructive && g.destructive} />
            }) }
        </div>
    };
    let Some(fleet) = props.fleet.as_ref().filter(|f| !f.hosts.is_empty()) else {
        return html! {
            <ContextProvider<Phases> context={props.phases.clone()}>{ by_plan_view }</ContextProvider<Phases>>
        };
    };
    let tab = |label: &'static str, selected: bool, value: bool| {
        let by_plan = by_plan.clone();
        html! {
            <button type="button" role="tab" aria-selected={selected.to_string()}
                class={classes!("tabs__tab", selected.then_some("tabs__tab--active"))}
                onclick={Callback::from(move |_| by_plan.set(value))}>
                { label }
            </button>
        }
    };
    html! {
        <ContextProvider<Phases> context={props.phases.clone()}>
            <FleetWarnings plan={fleet.clone()} />
            <div class="tabs tabs--small" role="tablist" aria-label="Plan view">
                { tab("Across the fleet", !*by_plan, false) }
                { tab("By distinct plan", *by_plan, true) }
            </div>
            if *by_plan { { by_plan_view } }
            else { <FleetPlanView plan={fleet.clone()} /> }
        </ContextProvider<Phases>>
    }
}
