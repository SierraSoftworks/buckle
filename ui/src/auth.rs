//! The browser's half of the server-driven OIDC session (design §12).
//!
//! Signing in is a full-page navigation to `/api/v1/auth/login`; the browser
//! never handles a token. When the session cookie lapses the server can renew
//! it from its `HttpOnly` refresh cookie, so [`refresh_session`] only asks.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use futures::FutureExt;
use futures::future::{LocalBoxFuture, Shared};

use crate::util::{enc, window};

/// Collapses concurrent runs of an async operation onto one, so a page whose
/// parallel fetches all meet the same expired session produces one refresh.
/// That matters because a provider may rotate the refresh token on every
/// redemption: had each request redeemed it, only the first would succeed and
/// the rest would invalidate the session it had just renewed.
/// The run in flight, tagged with the generation that started it.
type Slot<T> = Rc<RefCell<Option<(u64, Shared<LocalBoxFuture<'static, T>>)>>>;

struct SingleFlight<T: Clone> {
    current: Slot<T>,
    generation: Rc<Cell<u64>>,
}

impl<T: Clone + 'static> SingleFlight<T> {
    fn new() -> Self {
        Self {
            current: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
        }
    }

    fn run<F>(&self, start: F) -> impl Future<Output = T> + use<F, T>
    where
        F: FnOnce() -> LocalBoxFuture<'static, T>,
    {
        let (current, generation) = (self.current.clone(), self.generation.clone());
        async move {
            let (mine, shared) = {
                let mut slot = current.borrow_mut();
                match slot.as_ref() {
                    Some((g, existing)) => (*g, existing.clone()),
                    None => {
                        let g = generation.get().wrapping_add(1);
                        generation.set(g);
                        let shared = start().shared();
                        *slot = Some((g, shared.clone()));
                        (g, shared)
                    }
                }
            };
            let result = shared.await;
            let mut slot = current.borrow_mut();
            if matches!(slot.as_ref(), Some((g, _)) if *g == mine) {
                *slot = None;
            }
            result
        }
    }
}

thread_local! {
    static REFRESH: SingleFlight<Result<(), ()>> = SingleFlight::new();
    static REDIRECTING: Cell<bool> = const { Cell::new(false) };
}

/// Renews the session from its refresh cookie; `Ok` means the failed request
/// can be retried.
pub async fn refresh_session() -> Result<(), ()> {
    REFRESH
        .with(|f| f.run(|| async { crate::api::refresh().await.map_err(|_| ()) }.boxed_local()))
        .await
}

/// Where the browser is now, to come back to after signing in.
pub fn here() -> String {
    let location = window().location();
    let path = location.pathname().unwrap_or_else(|_| "/".into());
    let search = location.search().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    format!("{path}{search}{hash}")
}

/// The sign-in URL that comes back to `return_to` (the server's
/// `GET /auth/login?return_to=` parameter, a local path).
pub fn login_url(return_to: &str) -> String {
    format!(
        "{}/auth/login?return_to={}",
        crate::api::BASE,
        enc(return_to)
    )
}

/// Sends the browser to sign in, once, however many requests noticed.
pub fn begin_login() {
    if REDIRECTING.with(|r| r.replace(true)) {
        return;
    }
    let _ = window().location().assign(&login_url(&here()));
}

/// Ends the server session and goes back to the start.
pub async fn sign_out() {
    let _ = crate::api::logout().await;
    let _ = window().location().assign(&crate::util::demo_href("/"));
}
