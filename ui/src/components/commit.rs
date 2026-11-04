//! A commit of the configuration repository, the same wherever one is shown
//! (Revisions, the Overview, a rollout's head): its subject with a check
//! mark for a valid commit (a cross for a rejected one), who wrote it and
//! when, and its rollout. The whole card opens that rollout; `actions` are
//! its rollout controls, at the right of the commit hash.

use buckle_api::{RevisionStatus, RevisionSummary, RolloutSummary};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::icons;
use super::link::RouteLink;
use super::ui::Copyable;
use crate::app::Route;
use crate::logic::format::{
    commit, count, first_line, relative, rollout_label, rollout_note, timestamp,
};
use crate::logic::tone::{revision_status_tone, rollout_state_label, rollout_state_tone};

/// What a card shows of a commit: a revision where one is known, else what a
/// rollout carries of its own.
#[derive(Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub message: String,
    pub author: Option<String>,
    pub at: Option<DateTime<Utc>>,
    pub status: Option<RevisionStatus>,
    pub errors: usize,
    pub modules: Option<u32>,
}

impl From<&RevisionSummary> for Commit {
    fn from(r: &RevisionSummary) -> Self {
        Commit {
            sha: r.commit.clone(),
            message: r.message.clone(),
            author: Some(r.author.clone()),
            at: Some(r.created_at),
            status: Some(r.status),
            errors: r.validation_errors.len(),
            modules: Some(r.module_count),
        }
    }
}

impl Commit {
    /// A rollout's commit: its revision from `revisions` where listed.
    pub fn of_rollout(s: &RolloutSummary, revisions: &[RevisionSummary]) -> Self {
        revisions
            .iter()
            .find(|r| r.id == s.revision)
            .map(Commit::from)
            .unwrap_or_else(|| Commit {
                sha: s.revision.clone(),
                message: s.revision_message.clone(),
                author: None,
                at: None,
                // A rollout only starts from a valid revision.
                status: Some(RevisionStatus::Valid),
                errors: 0,
                modules: None,
            })
    }
}

#[derive(Properties, PartialEq)]
pub struct CommitCardProps {
    pub commit: Commit,
    /// The commit's rollout: the card opens it and says how it stands
    /// (unless `link` is off).
    #[prop_or_default]
    pub rollout: Option<RolloutSummary>,
    /// Whether the card opens the rollout (not on that rollout's own page).
    #[prop_or(true)]
    pub link: bool,
    /// Whether the meta line names the rollout and its state (a log that
    /// shows the rollout's progress itself turns it off).
    #[prop_or(true)]
    pub say_rollout: bool,
    /// The message's body under its subject.
    #[prop_or_default]
    pub body: bool,
    /// Standing alone, in a bordered card, rather than as a row of a list.
    #[prop_or_default]
    pub boxed: bool,
    pub now: DateTime<Utc>,
    /// Buttons at the card's right.
    #[prop_or_default]
    pub actions: Html,
    /// Below the meta line (a rejected commit's errors).
    #[prop_or_default]
    pub children: Html,
}

#[function_component(CommitCard)]
pub fn commit_card(props: &CommitCardProps) -> Html {
    let c = &props.commit;
    let subject = first_line(&c.message).to_string();
    let rejected = c.status == Some(RevisionStatus::Rejected);
    let target = props.rollout.as_ref().filter(|_| props.link);
    let status = c.status.map(|s| {
        let (icon, label, title) = match s {
            RevisionStatus::Valid => (icons::check(), "valid", "Valid: passed validation".to_string()),
            RevisionStatus::Rejected => (
                icons::close(),
                "rejected",
                format!("Rejected: {}", count(c.errors.max(1), "validation error", "validation errors")),
            ),
        };
        html! {
            <span class={classes!("commit__status", revision_status_tone(s).class())} aria-label={label} {title}>{ icon }</span>
        }
    });
    let body = c.message.lines().skip(1).collect::<Vec<_>>().join("\n");
    html! {
        <div class={classes!("commit", target.is_some().then_some("commit--link"), props.boxed.then_some("commit--boxed"), rejected.then_some("commit--rejected"))}
            data-commit={c.sha.clone()}>
            <div class="commit__main">
                <div class="commit__line">
                    { match target {
                        Some(r) => html! {
                            <RouteLink to={Route::rollout(&r.id)} classes="commit__subject"
                                title={format!("Open rollout {}", rollout_label(&r.id))}>{ subject }</RouteLink>
                        },
                        None => html! { <span class="commit__subject">{ subject }</span> },
                    } }
                    { status.unwrap_or_default() }
                </div>
                <div class="commit__meta muted">
                    if let Some(a) = &c.author { <span class="commit__author">{ a.split(" <").next().unwrap_or(a).to_string() }</span> }
                    if let Some(at) = c.at { <span title={timestamp(at)}>{ relative(at, props.now) }</span> }
                    if let Some(m) = c.modules { <span>{ count(m as usize, "module", "modules") }</span> }
                    // On its rollout's own page, the rollout goes without saying.
                    { match props.rollout.as_ref().filter(|_| props.link && props.say_rollout) {
                        Some(r) => html! {
                            <span class={classes!("commit__rollout", rollout_state_tone(r.state).class())} title={rollout_note(r)}>
                                { format!("rollout {} {}", rollout_label(&r.id), rollout_state_label(r.state)) }
                            </span>
                        },
                        None if props.link && props.say_rollout && !rejected && c.at.is_some() => html! { <span>{ "not rolled out" }</span> },
                        None => html! {},
                    } }
                </div>
                if props.body && !body.trim().is_empty() { <p class="commit__body">{ body.trim().to_string() }</p> }
                { props.children.clone() }
            </div>
            <div class="commit__aside">
                <Copyable text={c.sha.clone()} label={commit(&c.sha).to_string()} />
                if props.actions != Html::default() { <div class="commit__actions">{ props.actions.clone() }</div> }
            </div>
        </div>
    }
}
