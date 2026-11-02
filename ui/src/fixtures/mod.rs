//! Baked-in sample data for offline previews and the end-to-end tests.
//!
//! Appending `?demo` to any URL makes every API call answer from an
//! in-memory stand-in for the server ([`store`]) instead of the network, so
//! the whole UI can be walked with no server running. The substitution is
//! made inside [`crate::api`], so a page written against the real client
//! works in demo mode without knowing demo mode exists.
//!
//! Flags beside `?demo` choose a scenario:
//!
//! | flag | effect |
//! |---|---|
//! | `role=viewer` | signed in without admin rights: admin actions are hidden and refused |
//! | `auth=off` | `/me` answers anonymous, as a server without OIDC does (with `role=viewer`: one whose `admin_acl` does not match) |
//! | `auth=expired` | `/me` answers 401 and the refresh fails, so the UI sends you to sign in |
//! | `slow` | every call takes 700 ms, to see the loading states |
//! | `live` | the active rollout moves on by itself through the live event path |
//! | `empty` | an empty fleet with no rollouts, to see the empty states |
//!
//! The choice is remembered for the tab (`sessionStorage`), so a reload or a
//! deep link opened in the same tab stays in demo mode; `?demo=off` leaves.
//!
//! Demo mode is a development convenience, so everything beyond the flag is
//! compiled into debug builds only: a release bundle carries no fixtures.

#[cfg(debug_assertions)]
mod data;
#[cfg(debug_assertions)]
pub mod live;
#[cfg(debug_assertions)]
pub mod store;

#[cfg(debug_assertions)]
mod flags {
    use std::cell::RefCell;

    const KEY: &str = "buckle.demo";

    thread_local! {
        /// The demo query string, resolved once per page load.
        static QUERY: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
    }

    fn resolve() -> Option<String> {
        let window = web_sys::window()?;
        let search = window.location().search().ok().unwrap_or_default();
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
        let session = window.session_storage().ok().flatten();
        if let Some(value) = params.get("demo") {
            if value == "off" {
                if let Some(s) = &session {
                    let _ = s.remove_item(KEY);
                }
                return None;
            }
            let query = search.trim_start_matches('?').to_string();
            if let Some(s) = &session {
                let _ = s.set_item(KEY, &query);
            }
            return Some(query);
        }
        session.and_then(|s| s.get_item(KEY).ok().flatten())
    }

    pub fn query() -> Option<String> {
        QUERY.with(|q| q.borrow_mut().get_or_insert_with(resolve).clone())
    }
}

/// Whether this page load is in demo mode.
#[cfg(debug_assertions)]
pub fn is_demo() -> bool {
    flags::query().is_some()
}

#[cfg(not(debug_assertions))]
pub fn is_demo() -> bool {
    false
}

/// A flag beside `?demo` (`role=viewer` → `flag("role") == Some("viewer")`;
/// a bare `slow` → `Some("")`).
#[cfg(debug_assertions)]
pub fn flag(name: &str) -> Option<String> {
    let query = flags::query()?;
    web_sys::UrlSearchParams::new_with_str(&query)
        .ok()?
        .get(name)
}

/// Waits as long as the `slow` flag asks.
#[cfg(debug_assertions)]
pub async fn pause() {
    if flag("slow").is_some() {
        gloo_timers::future::TimeoutFuture::new(700).await;
    }
}

/// Serves a call from the demo store in demo mode. A macro so each call site
/// is one line above the real request, and so it vanishes from a release
/// build rather than relying on the optimiser.
macro_rules! demo {
    ($($body:tt)*) => {
        #[cfg(debug_assertions)]
        if $crate::fixtures::is_demo() {
            $crate::fixtures::pause().await;
            return { $($body)* };
        }
    };
}

pub(crate) use demo;
