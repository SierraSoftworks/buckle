//! Session hygiene the gRPC front owns: one cancellation per live agent
//! session (so an operator's forget can close it), a per-address limiter for
//! bootstrap-token enrolment, and how long a new stream may take to say Hello.

use crate::web::auth::RateLimiter;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long a new session may stay silent before it must have sent Hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// Bootstrap enrolment attempts allowed per peer address per minute.
pub const ENROLMENT_ATTEMPTS_PER_MINUTE: u32 = 10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The cancellation token of every live agent session, by host.
#[derive(Debug, Default)]
pub struct LiveSessions {
    inner: Mutex<HashMap<String, (u64, CancellationToken)>>,
}

impl LiveSessions {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `session` as the live one for `host` and returns the token
    /// that ends it. A previous session of the same host is cancelled.
    pub fn register(&self, host: &str, session: u64) -> CancellationToken {
        let token = CancellationToken::new();
        if let Some((_, old)) = lock(&self.inner).insert(host.to_owned(), (session, token.clone()))
        {
            old.cancel();
        }
        token
    }

    /// Forgets `session` if it is still the live one for `host`.
    pub fn release(&self, host: &str, session: u64) {
        let mut map = lock(&self.inner);
        if map.get(host).is_some_and(|(id, _)| *id == session) {
            map.remove(host);
        }
    }

    /// Ends the live session of `host`, if any. Returns whether there was one.
    pub fn close(&self, host: &str) -> bool {
        match lock(&self.inner).remove(host) {
            Some((_, token)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Whether `host` has a live session.
    pub fn is_live(&self, host: &str) -> bool {
        lock(&self.inner).contains_key(host)
    }
}

/// What the gRPC front shares between calls.
pub struct SessionGuard {
    /// Live sessions, closable by host.
    pub sessions: std::sync::Arc<LiveSessions>,
    /// Bootstrap-token attempts per peer address.
    pub enrolment: RateLimiter,
    /// How long a stream may take to send Hello.
    pub hello_timeout: Duration,
    /// What each host last reported that the server knows otherwise (tags,
    /// reserved labels), so a standing disagreement is audited once and not
    /// on every reconnect.
    pub disagreements: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for SessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGuard")
            .field("hello_timeout", &self.hello_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for SessionGuard {
    fn default() -> Self {
        Self {
            sessions: Default::default(),
            enrolment: RateLimiter::new(ENROLMENT_ATTEMPTS_PER_MINUTE, Duration::from_secs(60)),
            hello_timeout: HELLO_TIMEOUT,
            disagreements: Default::default(),
        }
    }
}

impl SessionGuard {
    /// Counts one bootstrap-token attempt from `peer`; `false` when over the limit.
    pub fn allow_enrolment(&self, peer: Option<IpAddr>) -> bool {
        self.enrolment.allow(peer)
    }

    /// Notes what `key` (a host and what it reports) now disagrees with the
    /// server about; empty when it agrees. Returns whether that is a new
    /// disagreement worth auditing.
    pub fn disagrees(&self, key: &str, what: String) -> bool {
        let mut map = lock(&self.disagreements);
        if what.is_empty() {
            map.remove(key);
            return false;
        }
        map.insert(key.to_owned(), what.clone()).as_ref() != Some(&what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standing_disagreement_is_reported_once() {
        let guard = SessionGuard::default();
        assert!(guard.disagrees("pi-1/tags", "tag:z2".into()));
        assert!(!guard.disagrees("pi-1/tags", "tag:z2".into()));
        assert!(guard.disagrees("pi-1/tags", "tag:z1".into()), "a new claim");
        assert!(!guard.disagrees("pi-1/tags", String::new()));
        assert!(
            guard.disagrees("pi-1/tags", "tag:z1".into()),
            "it came back"
        );
    }

    #[test]
    fn closing_a_host_cancels_its_live_session_only() {
        let live = LiveSessions::new();
        let a = live.register("a", 1);
        let b = live.register("b", 2);
        assert!(live.close("a"));
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
        assert!(!live.close("a"), "already closed");
    }

    #[test]
    fn a_reconnect_supersedes_the_previous_session() {
        let live = LiveSessions::new();
        let first = live.register("a", 1);
        let second = live.register("a", 2);
        assert!(first.is_cancelled());
        live.release("a", 1);
        assert!(
            live.is_live("a"),
            "releasing a stale session keeps the new one"
        );
        live.release("a", 2);
        assert!(!live.is_live("a"));
        assert!(!second.is_cancelled());
    }
}
