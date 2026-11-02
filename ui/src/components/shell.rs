//! The chrome around every page (design §12): a 48 px app bar, a 220 px
//! sidebar that becomes a drawer on a narrow screen, and the page itself.

use yew::prelude::*;
use yew_router::prelude::*;

use super::icons;
use super::link::RouteLink;
use super::theme::{self, Theme};
use crate::app::{Route, Session, use_auth};
use crate::store::{Link, use_live};

#[derive(Properties, PartialEq)]
pub struct ShellProps {
    #[prop_or_default]
    pub children: Html,
}

/// A destination: its label, its icon and its route.
type NavItem = (&'static str, fn() -> Html, Route);

const NAV: &[NavItem] = &[
    ("Overview", icons::overview, Route::Overview),
    ("Rollouts", icons::rollouts, Route::Rollouts),
    ("Hosts", icons::hosts, Route::Hosts),
    ("Revisions", icons::revisions, Route::Revisions),
    ("Audit", icons::audit, Route::Audit),
    ("Reference", icons::reference, Route::Reference),
];

fn section(route: &Route) -> Route {
    match route {
        Route::Rollout { .. } => Route::Rollouts,
        Route::Host { .. } => Route::Hosts,
        other => other.clone(),
    }
}

#[function_component(Shell)]
pub fn shell(props: &ShellProps) -> Html {
    let route = use_route::<Route>().unwrap_or(Route::Overview);
    let open = use_state(|| false);
    let theme = use_state(theme::current);

    // Following a link closes the drawer, so the page it opened can be seen.
    {
        let open = open.clone();
        use_effect_with(route.clone(), move |_| {
            open.set(false);
            || ()
        });
    }

    let toggle_nav = {
        let open = open.clone();
        Callback::from(move |_: MouseEvent| open.set(!*open))
    };
    let close_nav = {
        let open = open.clone();
        Callback::from(move |_: MouseEvent| open.set(false))
    };
    let toggle_theme = {
        let theme = theme.clone();
        Callback::from(move |_: MouseEvent| {
            let next = theme.other();
            theme::set(next);
            theme.set(next);
        })
    };

    let current = section(&route);
    let nav = NAV.iter().map(|(label, icon, to)| {
        let active = *to == current;
        html! {
            <RouteLink to={to.clone()} current={active}
                classes={classes!("nav__item", active.then_some("nav__item--active"))}>
                <span class="nav__icon">{ icon() }</span>
                <span class="nav__label">{ *label }</span>
            </RouteLink>
        }
    });

    html! {
        <div class="app">
            <a class="skip-link" href="#main">{ "Skip to content" }</a>
            <header class="app-bar">
                <button type="button" class="icon-btn app-bar__menu" onclick={toggle_nav}
                    aria-label={if *open { "Close navigation" } else { "Open navigation" }}
                    aria-controls="sidebar" aria-expanded={(*open).to_string()}>
                    { if *open { icons::close() } else { icons::menu() } }
                </button>
                <RouteLink to={Route::Overview} classes="app-bar__brand" label="Buckle overview">
                    { icons::logo() }<span>{ "Buckle" }</span>
                </RouteLink>
                if crate::fixtures::is_demo() {
                    <span class="badge badge--info" title="Every answer comes from the built-in fixtures. Add ?demo=off to leave.">{ "demo" }</span>
                }
                <span class="app-bar__spacer" />
                <LinkStatus />
                <button type="button" class="icon-btn" onclick={toggle_theme} data-testid="theme-toggle"
                    aria-label={format!("Switch to the {} theme", theme.other().name())}
                    title={format!("Switch to the {} theme", theme.other().name())}>
                    { if *theme == Theme::Dark { icons::sun() } else { icons::moon() } }
                </button>
                <UserChip />
            </header>
            <div class="app-body">
                if *open {
                    <div class="scrim" aria-hidden="true" onclick={close_nav} />
                }
                <aside id="sidebar" class={classes!("sidebar", (*open).then_some("sidebar--open"))}>
                    <nav class="nav" aria-label="Sections">{ for nav }</nav>
                </aside>
                <main class="app-main" id="main" tabindex="-1">
                    { props.children.clone() }
                </main>
            </div>
        </div>
    }
}

/// Whether live updates are flowing.
#[function_component(LinkStatus)]
fn link_status() -> Html {
    let live = use_live();
    let (class, label, title) = match live.link {
        Link::Open => (
            "live--open",
            "live".to_string(),
            "Receiving live updates.".to_string(),
        ),
        Link::Connecting => (
            "live--connecting",
            "connecting".into(),
            "Connecting to live updates…".into(),
        ),
        Link::Retrying(s) => (
            "live--retrying",
            "reconnecting".into(),
            format!(
                "Live updates were interrupted; trying again in {s}s. Pages refresh when they resume."
            ),
        ),
        Link::Demo => (
            "live--demo",
            "fixtures".into(),
            "Demo mode: updates come from the built-in fixtures.".into(),
        ),
    };
    html! {
        <span class={classes!("live", class)} title={title.clone()} role="status" aria-label={title}>
            <span class="live__dot" aria-hidden="true" />
            <span class="live__label">{ label }</span>
        </span>
    }
}

#[function_component(UserChip)]
fn user_chip() -> Html {
    let auth = use_auth();
    let sign_out = Callback::from(|_: MouseEvent| {
        wasm_bindgen_futures::spawn_local(crate::auth::sign_out());
    });
    match &auth.session {
        Session::SignedIn(me) => html! {
            <div class="user">
                <span class="user__avatar" aria-hidden="true">{ initials(&me.name) }</span>
                <span class="user__meta">
                    <span class="user__name">{ me.name.clone() }</span>
                    <span class="user__role">{ if me.is_admin { "admin" } else { "read only" } }</span>
                </span>
                <button type="button" class="btn btn--subtle btn--small" onclick={sign_out}>{ "Sign out" }</button>
            </div>
        },
        Session::Open { admin: true } => html! {
            <span class="badge badge--warn" title="This server has no sign-in configured; every action is permitted (admin_acl, when set, still sees request metadata).">
                { "no sign-in" }
            </span>
        },
        Session::Open { admin: false } => html! {
            <span class="user">
                <span class="badge badge--warn" title="This server has no sign-in configured; its admin_acl does not match this request's metadata, so actions are refused.">
                    { "no sign-in" }
                </span>
                <span class="user__role">{ "read only" }</span>
            </span>
        },
        _ => html! {},
    }
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}
