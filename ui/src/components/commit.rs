//! A commit of the configuration repository, the same wherever one is shown
//! (Rollouts, the Overview, a rollout's head): its rollout's name in the
//! rollout's colour, then its subject with a check mark for a valid commit
//! (a cross for a rejected one), who wrote it and when. The whole card opens
//! that rollout; `actions` are its rollout controls, at the right of the
//! commit hash. The body is re-flowed to the card's width, its trailers
//! become co-authors (Claude's linking its sessions), and a long one
//! collapses behind "Read more".

use buckle_api::{RevisionStatus, RevisionSummary, RolloutSummary};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::avatar::{Avatar, split_author};
use super::icons;
use super::link::RouteLink;
use super::ui::{Copyable, HoverTip};
use crate::app::Route;
use crate::logic::format::{commit, count, relative, rollout_label, timestamp};
use crate::logic::message::{Block, Message, is_claude};
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
    pub workloads: Option<u32>,
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
            workloads: Some(r.workload_count),
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
                workloads: None,
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
    let msg = use_memo(c.message.clone(), |m| Message::parse(m));
    let open = use_state(|| false);
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
    // The rollout's name leads the subject, in the rollout's colour.
    let name = props.rollout.as_ref().map(|r| html! {
        <span class={classes!("commit__rollout", rollout_state_tone(r.state).class())}
            title={format!("Rollout {} {}", rollout_label(&r.id), rollout_state_label(r.state))}>
            { rollout_label(&r.id) }
        </span>
    });
    let sizes = [
        c.modules.map(|m| count(m as usize, "module", "modules")),
        c.workloads
            .filter(|w| *w > 0)
            .map(|w| count(w as usize, "workload", "workloads")),
    ];
    let subject = msg.subject.clone();
    // Claude's sessions hang from the first Claude among the authors.
    let everyone = msg.authors(c.author.as_deref());
    let claude = everyone.iter().position(|a| is_claude(a));
    let authors = everyone.iter().enumerate().map(|(i, a)| {
        let sessions = if Some(i) == claude {
            msg.sessions.as_slice()
        } else {
            &[]
        };
        author(a, sessions)
    });
    let body = props.body.then(|| {
        let collapsed = msg.long() && !*open;
        let toggle = {
            let open = open.clone();
            Callback::from(move |_: MouseEvent| open.set(!*open))
        };
        let blocks = if collapsed { &msg.body[..msg.preview()] } else { &msg.body[..] };
        html! {
            if !msg.body.is_empty() || !msg.trailers.is_empty() {
                <div class={classes!("commit__body", collapsed.then_some("commit__body--collapsed"))}>
                    <div class="commit__text">
                        { for blocks.iter().map(block) }
                        if !collapsed && !msg.trailers.is_empty() {
                            <dl class="commit__trailers muted">
                                { for msg.trailers.iter().map(|(k, v)| html! { <div><dt>{ k.clone() }</dt><dd>{ v.clone() }</dd></div> }) }
                            </dl>
                        }
                    </div>
                    if msg.long() {
                        <button type="button" class="commit__more" aria-expanded={(!collapsed).to_string()} onclick={toggle}>
                            { if collapsed { "Read more…" } else { "Show less" } }
                        </button>
                    }
                </div>
            }
        }
    });
    html! {
        <div class={classes!("commit", target.is_some().then_some("commit--link"), props.boxed.then_some("commit--boxed"), rejected.then_some("commit--rejected"))}
            data-commit={c.sha.clone()}>
            <div class="commit__main">
                <div class="commit__line">
                    { match target {
                        Some(r) => html! {
                            <RouteLink to={Route::rollout(&r.id)} classes="commit__subject"
                                title={format!("Open rollout {}", rollout_label(&r.id))}>{ name.unwrap_or_default() }{ subject }</RouteLink>
                        },
                        None => html! { <span class="commit__subject">{ name.unwrap_or_default() }{ subject }</span> },
                    } }
                    { status.unwrap_or_default() }
                </div>
                <div class="commit__meta muted">
                    { for authors }
                    if let Some(at) = c.at { <span title={timestamp(at)}>{ relative(at, props.now) }</span> }
                    { for sizes.into_iter().flatten().map(|s| html! { <span>{ s }</span> }) }
                </div>
                { body.unwrap_or_default() }
                { props.children.clone() }
            </div>
            <div class="commit__aside">
                <Copyable text={c.sha.clone()} label={commit(&c.sha).to_string()} />
                if props.actions != Html::default() { <div class="commit__actions">{ props.actions.clone() }</div> }
            </div>
        </div>
    }
}

/// An author in the meta line; Claude's entry opens its session, or, with
/// several, lists them on hover.
fn author(a: &str, sessions: &[String]) -> Html {
    let (name, email) = split_author(a);
    let title = email.map(|e| format!("{name} <{e}>"));
    let face = html! { <><Avatar author={a.to_string()} />{ name.to_string() }</> };
    match sessions {
        [] => html! { <span class="commit__author" {title}>{ face }</span> },
        [one] => html! {
            <a class="commit__author commit__author--link" href={one.clone()} target="_blank" rel="noopener noreferrer"
                title={format!("{name}: open its session")}>{ face }</a>
        },
        many => {
            let tip = html! { <>
                <span class="tip__head">{ format!("{name}: {} sessions", many.len()) }</span>
                { for many.iter().map(|s| html! {
                    <a class="commit__session mono" href={s.clone()} target="_blank" rel="noopener noreferrer">{ session_label(s) }</a>
                }) }
            </> };
            html! {
                <span class="commit__author commit__author--link">
                    <HoverTip {tip} label={format!("{name}: {} sessions", many.len())}>{ face }</HoverTip>
                </span>
            }
        }
    }
}

/// A session link's last path segment: `…/code/session_01Ab` → `session_01Ab`.
fn session_label(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn block(b: &Block) -> Html {
    match b {
        Block::Para(t) => html! { <p>{ t.clone() }</p> },
        Block::List {
            ordered: false,
            items,
        } => html! { <ul>{ for items.iter().map(|i| html! { <li>{ i.clone() }</li> }) }</ul> },
        Block::List {
            ordered: true,
            items,
        } => html! { <ol>{ for items.iter().map(|i| html! { <li>{ i.clone() }</li> }) }</ol> },
        Block::Code(t) => html! { <pre><code>{ t.clone() }</code></pre> },
        Block::Rule => html! { <hr /> },
    }
}
