//! The revision log shared by Rollouts and the Overview: each commit as a
//! [`CommitCard`] with its rollout's progress ([`RolloutProgress`]) inside
//! it and, at the right of the commit hash, the one control (or marker)
//! that fits where it stands.

use buckle_api::{RevisionStatus, RevisionSummary, RolloutSummary};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use crate::components::error_block::ErrorBlock;
use crate::components::ui::Note;
use yew_router::prelude::*;

use super::changes::Took;
use super::meter::rollout_segments;
use super::{
    Button, Changes, Commit, CommitCard, Confirm, Kind, Meter, Pill, RouteLink, use_toast,
};
use crate::api;
use crate::app::{Route, use_auth};
use crate::logic::controls::{Stance, stance};
use crate::logic::fleet_plan::Tally;
use crate::logic::format::{
    commit, count, first_line, relative, rollout_label, rollout_note, short_duration, timestamp,
};
use crate::logic::tone::{
    rollout_active, rollout_finished, rollout_state_label, rollout_state_tone,
};

/// What an operator can ask of a revision.
#[derive(Clone, PartialEq)]
pub enum RevisionAsk {
    Start(RevisionSummary),
    RollBack(RevisionSummary),
}

/// The state behind a revision's Start and Roll back buttons and the dialog
/// that confirms them.
pub struct RevisionActions {
    ask: Option<RevisionAsk>,
    busy: bool,
    error: Option<String>,
    pub open: Callback<RevisionAsk>,
    close: Callback<()>,
    run: Callback<String>,
}

#[hook]
pub fn use_revision_actions() -> RevisionActions {
    let toast = use_toast();
    let navigator = use_navigator();
    let ask = use_state(|| None::<RevisionAsk>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let open = {
        let (ask, error) = (ask.clone(), error.clone());
        Callback::from(move |a: RevisionAsk| {
            error.set(None);
            ask.set(Some(a));
        })
    };
    let close = {
        let ask = ask.clone();
        Callback::from(move |_| ask.set(None))
    };
    let run = {
        let (ask, busy, error) = (ask.clone(), busy.clone(), error.clone());
        Callback::from(move |note: String| {
            let Some(what) = (*ask).clone() else { return };
            let rev = match &what {
                RevisionAsk::Start(r) | RevisionAsk::RollBack(r) => r.clone(),
            };
            let note = match &what {
                RevisionAsk::RollBack(_) if note.is_empty() => {
                    format!("Roll the fleet back to {}", commit(&rev.commit))
                }
                _ => note,
            };
            let (ask, busy, error, toast, navigator) = (
                ask.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                navigator.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::start_rollout(&rev.id, &note).await {
                    Ok(r) => {
                        toast.ok(format!(
                            "Rollout {} of {} started.",
                            rollout_label(&r.id),
                            commit(&rev.commit)
                        ));
                        ask.set(None);
                        if let Some(n) = navigator {
                            n.push(&Route::rollout(&r.id));
                        }
                    }
                    Err(e) => {
                        if e.is_forbidden() {
                            toast.error(format!("Not permitted: {e}"));
                        }
                        error.set(Some(e.to_string()));
                    }
                }
                busy.set(false);
            });
        })
    };
    RevisionActions {
        ask: (*ask).clone(),
        busy: *busy,
        error: (*error).clone(),
        open,
        close,
        run,
    }
}

impl RevisionActions {
    /// The open dialog, if one is. `rollouts` tells it whether another
    /// rollout is running.
    pub fn dialog(&self, rollouts: &[RolloutSummary]) -> Html {
        let Some(a) = &self.ask else {
            return html! {};
        };
        let queued = rollouts
            .iter()
            .find(|r| rollout_active(r.state))
            .map(|r| format!("Rollout {} is still running; depending on rollout.supersede the new one waits for it or replaces it.", rollout_label(&r.id)));
        let cancel = self.close.clone();
        match a {
            RevisionAsk::Start(r) => html! {
                <Confirm title={format!("Start a rollout of {}", commit(&r.commit))} confirm_label="Start rollout" note=true
                    busy={self.busy} error={self.error.clone()} on_confirm={self.run.clone()} on_cancel={cancel}>
                    <p>{ format!("“{}” moves through the phases as buckle.yaml describes: plans first, then every gate, wave and bake.", first_line(&r.message)) }</p>
                    if let Some(q) = queued { <Note>{ q }</Note> }
                </Confirm>
            },
            RevisionAsk::RollBack(r) => html! {
                <Confirm title={format!("Roll the fleet back to {}", commit(&r.commit))} confirm_label={format!("Roll back to {}", commit(&r.commit))}
                    danger=true note=true type_to_confirm={AttrValue::from(commit(&r.commit).to_string())}
                    busy={self.busy} error={self.error.clone()} on_confirm={self.run.clone()} on_cancel={cancel}>
                    <p>{ format!("Starts a rollout of the older revision “{}”. Rolling back is rolling forward to old content: resources added since are removed, and every phase gate, approval and bake still applies.", first_line(&r.message)) }</p>
                    <p class="muted">{ "An irreversible task (a package upgrade with a schema migration) only goes back as far as its kind can; the plans will show exactly what will happen." }</p>
                    if let Some(q) = queued { <Note>{ q }</Note> }
                </Confirm>
            },
        }
    }
}

#[derive(Properties, PartialEq)]
pub struct RevisionControlsProps {
    pub revision: RevisionSummary,
    pub rollout: Option<RolloutSummary>,
    pub stable_at: Option<DateTime<Utc>>,
    pub open: Callback<RevisionAsk>,
}

/// The rollout controls of a revision card: [Stable] for the current stable
/// release, a red [Corrupt] for a revision that failed validation, an orange
/// Rollback for an older one, a blue Review for a
/// proposed rollout awaiting approval, and a blue [Running] marker for a
/// rollout in progress that needs nothing. A newer revision nobody has
/// rolled out gets a Start button.
#[function_component(RevisionControls)]
pub fn revision_controls(props: &RevisionControlsProps) -> Html {
    let auth = use_auth();
    let admin = auth.is_admin();
    let rev = &props.revision;
    let ask = |a: RevisionAsk| {
        let open = props.open.clone();
        Callback::from(move |_: MouseEvent| open.emit(a.clone()))
    };
    match stance(rev, props.rollout.as_ref(), props.stable_at) {
        None => html! {},
        Some(Stance::Corrupt) => html! {
            <>
                if admin { <Revalidate revision={rev.clone()} /> }
                <span class="marker tone-danger" title="Rejected by validation: this revision cannot be deployed">{ "Corrupt" }</span>
            </>
        },
        Some(Stance::Stable) => html! {
            <span class="marker tone-ok" title="The release the fleet is on">{ "Stable" }</span>
        },
        Some(Stance::Running) => html! {
            <span class="marker tone-active" title="A rollout of this revision is in progress">{ "Running" }</span>
        },
        Some(Stance::Review) => match &props.rollout {
            Some(r) => html! {
                <yewi::Button size={yewi::Size::Small} variant={yewi::ButtonVariant::Primary} href={yewi::NavTarget::route(Route::rollout(&r.id))}
                attrs={yewi::attrs! { "title" => "This rollout is waiting for an approval" }}>{ "Review…" }</yewi::Button>
            },
            None => html! {},
        },
        Some(Stance::Rollback) if admin => html! {
            <Button small=true kind={Kind::Caution} title="Roll the fleet back to this revision"
                onclick={ask(RevisionAsk::RollBack(rev.clone()))}>{ "Rollback…" }</Button>
        },
        Some(Stance::Start) if admin => html! {
            <Button small=true onclick={ask(RevisionAsk::Start(rev.clone()))}>{ "Start rollout…" }</Button>
        },
        Some(_) => html! {},
    }
}

#[derive(Properties, PartialEq)]
struct RevalidateProps {
    revision: RevisionSummary,
}

/// Validates a rejected revision again: a server upgrade may accept what an
/// older one refused. The card updates from the live event.
#[function_component(Revalidate)]
fn revalidate(props: &RevalidateProps) -> Html {
    let toast = use_toast();
    let busy = use_state(|| false);
    let onclick = {
        let (busy, toast, rev) = (busy.clone(), toast.clone(), props.revision.clone());
        Callback::from(move |_: MouseEvent| {
            let (busy, toast, rev) = (busy.clone(), toast.clone(), rev.clone());
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::revalidate(&rev.id).await {
                    Ok(r) if r.status == RevisionStatus::Valid => {
                        toast.ok(format!("{} is valid now.", commit(&r.commit)))
                    }
                    Ok(r) => toast.error(format!(
                        "{} is still rejected: {}",
                        commit(&r.commit),
                        r.validation_errors
                            .first()
                            .map(String::as_str)
                            .unwrap_or("")
                    )),
                    Err(e) => toast.error(e.to_string()),
                }
                busy.set(false);
            });
        })
    };
    html! {
        <Button small=true busy={*busy} {onclick}
            title="Validate this revision again with the server's current rules">{ "Re-validate" }</Button>
    }
}

#[derive(Properties, PartialEq)]
pub struct RolloutProgressProps {
    pub rollout: RolloutSummary,
    pub now: DateTime<Utc>,
}

/// How a rollout is going, the same line on the Overview and Rollouts: its
/// state, what it changes, the stage it is in and how many of its targets
/// are done, then how long it took.
#[function_component(RolloutProgress)]
pub fn rollout_progress(props: &RolloutProgressProps) -> Html {
    let r = &props.rollout;
    let phase = r
        .current_phase
        .as_ref()
        .filter(|_| !rollout_finished(r.state));
    let took = r
        .finished_at
        .map(|f| short_duration(f.signed_duration_since(r.started_at).num_seconds()));
    let started = format!(
        "Started by {} {} ({})",
        r.triggered_by,
        relative(r.started_at, props.now),
        timestamp(r.started_at)
    );
    let title = match rollout_note(r) {
        Some(note) => format!("{note}\n{started}"),
        None => started,
    };
    html! {
        <div class="rollout-line" data-rollout={r.id.clone()}>
            <Pill tone={rollout_state_tone(r.state)} label={rollout_state_label(r.state)} {title} />
            if let Some(c) = r.changes { <Changes tally={Tally::from(c)} /> }
            if let Some(p) = phase { <span class="muted">{ format!("in {p}") }</span> }
            if r.hosts_total > 0 {
                <span class="rollout-line__progress">
                    <Meter segments={rollout_segments(r)} total={r.hosts_total as usize} class="rollout-line__meter" />
                    <span class="muted">
                        { format!("{}/{} done", r.hosts_done, r.hosts_total) }
                        if r.hosts_failed > 0 { <span class="text-danger">{ format!(" · {} failed", r.hosts_failed) }</span> }
                    </span>
                </span>
            }
            if let Some(t) = took { <Took label={t} /> }
            if let Some(by) = &r.superseded_by {
                <span class="muted">{ "superseded by " }<RouteLink to={Route::rollout(by)}>{ rollout_label(by) }</RouteLink></span>
            }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct RevisionLogProps {
    pub revisions: Vec<RevisionSummary>,
    pub rollouts: Vec<RolloutSummary>,
    pub now: DateTime<Utc>,
    pub open: Callback<RevisionAsk>,
    /// Show at most this many, saying how many more there are.
    #[prop_or_default]
    pub limit: Option<usize>,
}

/// Revisions newest first on a git-log rail, each with its rollout.
#[function_component(RevisionLog)]
pub fn revision_log(props: &RevisionLogProps) -> Html {
    let stable_at = props
        .revisions
        .iter()
        .find(|r| r.is_stable)
        .map(|r| r.created_at);
    let shown = props.limit.unwrap_or(usize::MAX);
    html! {
        <ol class={classes!("gitlog", props.limit.is_some().then_some("gitlog--compact"))}>
            { for props.revisions.iter().take(shown).map(|r| {
                let rollout = r.rollout_id.as_ref().and_then(|id| props.rollouts.iter().find(|x| &x.id == id));
                revision_item(r, rollout, stable_at, props.now, &props.open)
            }) }
            if props.revisions.len() > shown {
                <li class="gitlog__more muted">{ format!("and {}", count(props.revisions.len() - shown, "older revision", "older revisions")) }</li>
            }
        </ol>
    }
}

/// One revision of the log: its commit card with its rollout's progress.
pub fn revision_item(
    r: &RevisionSummary,
    rollout: Option<&RolloutSummary>,
    stable_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    open: &Callback<RevisionAsk>,
) -> Html {
    let rejected = r.status == RevisionStatus::Rejected;
    html! {
        <li key={format!("rev-{}", r.id)} class={classes!("gitlog__item", rejected.then_some("gitlog__item--rejected"), r.is_stable.then_some("gitlog__item--stable"))} data-revision={r.id.clone()}>
            <span class="gitlog__dot" aria-hidden="true" />
            <CommitCard commit={Commit::from(r)} rollout={rollout.cloned()} body=true {now}
                actions={html! { <RevisionControls revision={r.clone()} rollout={rollout.cloned()} {stable_at} open={open.clone()} /> }}>
                if rejected {
                    <div class="errors" aria-label="Validation errors">
                        { for r.validation_errors.iter().map(|e| html! {
                            <ErrorBlock error={crate::logic::error_text::parse(e)} />
                        }) }
                        if r.validation_errors.is_empty() { <p class="muted">{ "Rejected without a reason recorded." }</p> }
                    </div>
                }
                { match rollout {
                    Some(x) => html! { <RolloutProgress rollout={x.clone()} {now} /> },
                    None if r.rollout_id.is_none() && !rejected => html! { <div class="rollout-line muted">{ "not rolled out" }</div> },
                    None => html! {},
                } }
            </CommitCard>
        </li>
    }
}
