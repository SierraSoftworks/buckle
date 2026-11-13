//! The chrome around every page (design §12), which is yewi's `AppShell`: an
//! app bar, a sidebar that becomes a drawer on a narrow screen, and the page
//! itself. The menu button opens the drawer on a narrow screen and hides or
//! shows the sidebar (remembered) on a wide one. The app bar carries the
//! page's search or filter box, so it stays put while the page scrolls.

use yew::prelude::*;
use yew_router::prelude::*;
use yewi::{
    AppBar, AppBarBrand, AppShell, IconData, NavTarget, Sidebar, SidebarItem, StatusDot,
    ThemeToggle, UserMenu, icons as yi,
};

use super::attention::AttentionBell;
use super::icons;
use super::search::AppSearch;
use super::ui::Badge;
use crate::app::{Route, Session, use_auth};
use crate::logic::tone::Tone;
use crate::store::{Link, use_live};

#[derive(Properties, PartialEq)]
pub struct ShellProps {
    #[prop_or_default]
    pub children: Html,
}

/// A destination: its label, its icon and its route.
type NavItem = (&'static str, IconData, Route);

const NAV: &[NavItem] = &[
    ("Overview", yi::LAYOUT_DASHBOARD, Route::Overview),
    (
        "Rollouts",
        yi::CHART_NO_AXES_COLUMN_DECREASING,
        Route::Rollouts,
    ),
    ("Hosts", yi::SERVER, Route::Hosts),
    ("Workloads", yi::LAYERS, Route::Workloads),
    ("Clusters", yi::WAYPOINTS, Route::Clusters),
    ("Audit", yi::SCROLL_TEXT, Route::Audit),
];

/// The documentation site: modules, tasks, the command line and runbooks.
pub const DOCS_URL: &str = "https://buckle.sierrasoftworks.com";

fn section(route: &Route) -> Route {
    match route {
        Route::Rollout { .. } => Route::Rollouts,
        Route::Host { .. } => Route::Hosts,
        Route::Cluster { .. } => Route::Clusters,
        Route::Workload { .. } => Route::Workloads,
        other => other.clone(),
    }
}

/// Where a wide screen remembers that its sidebar was hidden.
const NAV_COLLAPSED: &str = "buckle.nav.collapsed";

#[function_component(Shell)]
pub fn shell(props: &ShellProps) -> Html {
    let route = use_route::<Route>().unwrap_or(Route::Overview);
    let current = section(&route);

    let brand = html! {
        <AppBarBrand logo={icons::logo()} href={NavTarget::route(Route::Overview)} label="Buckle overview">
            { "Buckle" }
        </AppBarBrand>
    };
    let actions = html! { <>
        <LinkStatus />
        <AttentionBell />
        <ThemeToggle attrs={yewi::attrs! { "data-testid" => "theme-toggle" }} />
        <UserArea />
    </> };
    let bar = html! {
        <AppBar class="shell__bar" {brand} {actions} menu_label="Navigation">
            if crate::fixtures::is_demo() {
                <Badge tone={Tone::Info} title="Every answer comes from the built-in fixtures. Add ?demo=off to leave.">{ "demo" }</Badge>
            }
            <AppSearch />
        </AppBar>
    };
    let docs = html! {
        <SidebarItem href={NavTarget::href(DOCS_URL).external()} icon={yi::BOOK_OPEN}
            attrs={yewi::attrs! { "title" => "Modules, tasks, the command line and runbooks" }}>
            { "Documentation" }
        </SidebarItem>
    };
    let sidebar = html! {
        <Sidebar label="Sections" footer={docs}>
            { for NAV.iter().map(|(label, icon, to)| html! {
                <SidebarItem href={NavTarget::route(to.clone())} icon={*icon} current={*to == current}>
                    { *label }
                </SidebarItem>
            }) }
        </Sidebar>
    };

    html! {
        <AppShell class="shell" {bar} {sidebar} storage_key={NAV_COLLAPSED}>
            { props.children.clone() }
        </AppShell>
    }
}

/// Whether live updates are flowing.
#[function_component(LinkStatus)]
fn link_status() -> Html {
    let live = use_live();
    let (tone, active, label, title) = match live.link {
        Link::Open => (
            yewi::Tone::Ok,
            false,
            "live".to_string(),
            "Receiving live updates.".to_string(),
        ),
        Link::Connecting => (
            yewi::Tone::Neutral,
            true,
            "connecting".into(),
            "Connecting to live updates…".into(),
        ),
        Link::Retrying(s) => (
            yewi::Tone::Warn,
            true,
            "reconnecting".into(),
            format!(
                "Live updates were interrupted; trying again in {s}s. Pages refresh when they resume."
            ),
        ),
        Link::Demo => (
            yewi::Tone::Info,
            false,
            "fixtures".into(),
            "Demo mode: updates come from the built-in fixtures.".into(),
        ),
    };
    html! {
        <span class="live" title={title.clone()} role="status" aria-label={title}>
            <StatusDot {tone} {active} />
            <span class="live__label">{ label }</span>
        </span>
    }
}

/// Who is signed in, and the way out; or that nobody needs to be.
#[function_component(UserArea)]
fn user_area() -> Html {
    let auth = use_auth();
    match &auth.session {
        Session::SignedIn(me) => html! {
            <UserMenu name={me.name.clone()} show_name=true
                detail={if me.is_admin { "admin" } else { "read only" }}
                on_sign_out={Callback::from(|()| {
                    wasm_bindgen_futures::spawn_local(crate::auth::sign_out());
                })} />
        },
        Session::Open { admin: true } => html! {
            <Badge tone={Tone::Warn} title="This server has no sign-in configured; every action is permitted (admin_acl, when set, still sees request metadata).">
                { "no sign-in" }
            </Badge>
        },
        Session::Open { admin: false } => html! {
            <span class="user">
                <Badge tone={Tone::Warn} title="This server has no sign-in configured; its admin_acl does not match this request's metadata, so actions are refused.">
                    { "no sign-in" }
                </Badge>
                <span class="user__role">{ "read only" }</span>
            </span>
        },
        _ => html! {},
    }
}
