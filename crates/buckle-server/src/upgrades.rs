//! Agent upgrades the server asks for (design §08, "Self-update").
//!
//! An admin asks for a host's agent to install a release, and the server
//! sends `Upgrade { when_idle: true }` on the host's session. The request is
//! remembered until the agent settles it:
//!
//! - it reconnects running the requested version (upgraded), or reports
//!   `UpgradeResult` up to date: the request is cleared;
//! - it reports `UpgradeResult` failed: the request is kept, failed, so the
//!   UI shows why;
//! - it reconnects still on another version (restarted before the upgrade ran,
//!   or an agent too old to report): the Upgrade is sent again, up to
//!   [`MAX_ATTEMPTS`] sessions, then the request counts as failed.
//!
//! Requests live in memory: a server restart forgets them, and each agent's
//! Hello still shows the version it ended up on.

use crate::rollout::HostLink;
use buckle_api::{AgentUpgrade, UpgradeState};
use buckle_proto::v1::{ServerMessage, Upgrade, UpgradeOutcome, server_message};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Sessions an Upgrade is sent on before an agent that keeps reconnecting on
/// its old version counts as failed.
pub const MAX_ATTEMPTS: u32 = 3;

/// Why an upgrade could not be requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// Not a release version.
    InvalidVersion(String),
    /// The agent already runs it.
    Current(String),
    /// The host has no session to send it on.
    Offline,
}

/// How a request was settled, for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    Upgraded { version: String },
    UpToDate { version: String },
    Failed { version: String, error: String },
}

struct Pending {
    upgrade: AgentUpgrade,
    attempts: u32,
}

/// The upgrades requested and not yet settled, by host.
pub struct AgentUpgrades {
    link: Arc<dyn HostLink>,
    desired: String,
    inner: Mutex<HashMap<String, Pending>>,
}

impl std::fmt::Debug for AgentUpgrades {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentUpgrades")
            .field("desired", &self.desired)
            .finish_non_exhaustive()
    }
}

impl Default for AgentUpgrades {
    /// No sessions to send on: every request finds the host offline.
    fn default() -> Self {
        Self::new(Arc::new(NoLink), env!("CARGO_PKG_VERSION"))
    }
}

struct NoLink;

impl HostLink for NoLink {
    fn send(&self, _: &str, _: ServerMessage) -> bool {
        false
    }
    fn is_connected(&self, _: &str) -> bool {
        false
    }
}

/// Whether `a` and `b` name the same release (`v2.3.0` is `2.3.0`).
pub fn same_version(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches('v') == b.trim().trim_start_matches('v')
}

/// Whether `v` looks like a release version: `[v]MAJOR.MINOR.PATCH[-pre][+build]`.
pub fn is_release_version(v: &str) -> bool {
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], &v[i + 1..]),
        None => (v, "1"),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
}

impl AgentUpgrades {
    /// Upgrades sent through `link`; `desired` is what a request without a
    /// version installs.
    pub fn new(link: Arc<dyn HostLink>, desired: impl Into<String>) -> Self {
        Self {
            link,
            desired: desired.into(),
            inner: Mutex::default(),
        }
    }

    /// The version a request without one installs.
    pub fn desired(&self) -> &str {
        &self.desired
    }

    /// The unsettled request for `host`.
    pub fn get(&self, host: &str) -> Option<AgentUpgrade> {
        self.lock().get(host).map(|p| p.upgrade.clone())
    }

    /// Asks `host`'s agent (running `running`) to install `version`, or the
    /// desired version. Replaces any earlier request.
    pub fn request(
        &self,
        host: &str,
        running: Option<&str>,
        version: Option<&str>,
        by: &str,
        now: DateTime<Utc>,
    ) -> Result<AgentUpgrade, RequestError> {
        let version = version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(&self.desired)
            .trim_start_matches('v')
            .to_owned();
        if !is_release_version(&version) {
            return Err(RequestError::InvalidVersion(version));
        }
        if running.is_some_and(|r| same_version(r, &version)) {
            return Err(RequestError::Current(version));
        }
        if !self.send(host, &version) {
            return Err(RequestError::Offline);
        }
        let upgrade = AgentUpgrade {
            version,
            state: UpgradeState::Requested,
            requested_at: now,
            requested_by: by.to_owned(),
            error: None,
        };
        self.lock().insert(
            host.to_owned(),
            Pending {
                upgrade: upgrade.clone(),
                attempts: 1,
            },
        );
        Ok(upgrade)
    }

    /// A new session from `host`, running `running`. Call once the session
    /// can receive messages.
    pub fn connected(&self, host: &str, running: &str) -> Option<Settled> {
        let mut map = self.lock();
        let pending = map.get_mut(host)?;
        let version = pending.upgrade.version.clone();
        if same_version(running, &version) {
            map.remove(host);
            return Some(Settled::Upgraded { version });
        }
        if pending.upgrade.state != UpgradeState::Requested {
            return None;
        }
        if pending.attempts >= MAX_ATTEMPTS {
            let error =
                format!("The agent reconnected {MAX_ATTEMPTS} times still running {running}.");
            pending.upgrade.state = UpgradeState::Failed;
            pending.upgrade.error = Some(error.clone());
            return Some(Settled::Failed { version, error });
        }
        pending.attempts += 1;
        drop(map);
        self.send(host, &version);
        None
    }

    /// `host` reported how an Upgrade to `version` went.
    pub fn reported(
        &self,
        host: &str,
        version: &str,
        outcome: UpgradeOutcome,
        error: &str,
    ) -> Option<Settled> {
        let mut map = self.lock();
        let pending = map.get_mut(host)?;
        if !same_version(&pending.upgrade.version, version) {
            return None;
        }
        let version = pending.upgrade.version.clone();
        match outcome {
            UpgradeOutcome::UpToDate => {
                map.remove(host);
                Some(Settled::UpToDate { version })
            }
            UpgradeOutcome::Failed => {
                let error = if error.is_empty() {
                    "The agent reported the upgrade failed.".to_owned()
                } else {
                    error.to_owned()
                };
                pending.upgrade.state = UpgradeState::Failed;
                pending.upgrade.error = Some(error.clone());
                Some(Settled::Failed { version, error })
            }
            // Restarting: the reconnect settles it.
            UpgradeOutcome::Updated | UpgradeOutcome::Unspecified => None,
        }
    }

    /// Drops any request for a forgotten host.
    pub fn forget(&self, host: &str) {
        self.lock().remove(host);
    }

    fn send(&self, host: &str, version: &str) -> bool {
        self.link.send(
            host,
            ServerMessage {
                msg: Some(server_message::Msg::Upgrade(Upgrade {
                    version: version.to_owned(),
                    when_idle: true,
                })),
            },
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Records what was sent; `online` hosts accept messages.
    #[derive(Default)]
    struct Recorder {
        online: Vec<&'static str>,
        sent: Mutex<Vec<(String, String)>>,
    }

    impl HostLink for Recorder {
        fn send(&self, host: &str, message: ServerMessage) -> bool {
            let Some(server_message::Msg::Upgrade(u)) = message.msg else {
                return false;
            };
            if !self.is_connected(host) {
                return false;
            }
            self.sent.lock().unwrap().push((host.into(), u.version));
            true
        }
        fn is_connected(&self, host: &str) -> bool {
            self.online.contains(&host)
        }
    }

    fn fixture() -> (Arc<Recorder>, AgentUpgrades) {
        let link = Arc::new(Recorder {
            online: vec!["pi-1"],
            ..Default::default()
        });
        (link.clone(), AgentUpgrades::new(link, "2.3.0"))
    }

    fn sent(link: &Recorder) -> Vec<String> {
        link.sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[rstest]
    #[case("2.3.0", true)]
    #[case("v2.3.0", true)]
    #[case("2.3.0-rc.1", true)]
    #[case("2.3", false)]
    #[case("latest", false)]
    #[case("2.3.0-", false)]
    #[case("2.3.0; rm -rf /", false)]
    fn release_versions(#[case] v: &str, #[case] ok: bool) {
        assert_eq!(is_release_version(v), ok);
    }

    #[rstest]
    #[case(None, "2.3.0")]
    #[case(Some(""), "2.3.0")]
    #[case(Some("v2.4.0"), "2.4.0")]
    fn requests_send_the_version_or_the_desired_one(
        #[case] version: Option<&str>,
        #[case] expected: &str,
    ) {
        let (link, upgrades) = fixture();
        let u = upgrades
            .request("pi-1", Some("2.2.0"), version, "ben", Utc::now())
            .unwrap();
        assert_eq!(u.version, expected);
        assert_eq!(u.state, UpgradeState::Requested);
        assert_eq!(sent(&link), vec![expected.to_owned()]);
        assert_eq!(upgrades.get("pi-1"), Some(u));
    }

    #[rstest]
    #[case("pi-1", Some("v2.3.0"), None, RequestError::Current("2.3.0".into()))]
    #[case("pi-1", None, Some("next"), RequestError::InvalidVersion("next".into()))]
    #[case("pi-2", Some("2.2.0"), None, RequestError::Offline)]
    fn refused_requests_send_nothing(
        #[case] host: &str,
        #[case] running: Option<&str>,
        #[case] version: Option<&str>,
        #[case] expected: RequestError,
    ) {
        let (link, upgrades) = fixture();
        let err = upgrades
            .request(host, running, version, "ben", Utc::now())
            .unwrap_err();
        assert_eq!(err, expected);
        assert!(sent(&link).is_empty());
        assert_eq!(upgrades.get(host), None);
    }

    #[test]
    fn reconnecting_on_the_new_version_settles_the_request() {
        let (_, upgrades) = fixture();
        upgrades
            .request("pi-1", Some("2.2.0"), None, "ben", Utc::now())
            .unwrap();
        assert_eq!(
            upgrades.connected("pi-1", "v2.3.0"),
            Some(Settled::Upgraded {
                version: "2.3.0".into()
            })
        );
        assert_eq!(upgrades.get("pi-1"), None);
    }

    #[test]
    fn reconnecting_on_the_old_version_resends_then_fails() {
        let (link, upgrades) = fixture();
        upgrades
            .request("pi-1", Some("2.2.0"), None, "ben", Utc::now())
            .unwrap();
        for _ in 1..MAX_ATTEMPTS {
            assert_eq!(upgrades.connected("pi-1", "2.2.0"), None);
        }
        assert_eq!(sent(&link).len(), MAX_ATTEMPTS as usize);
        assert!(matches!(
            upgrades.connected("pi-1", "2.2.0"),
            Some(Settled::Failed { .. })
        ));
        assert_eq!(sent(&link).len(), MAX_ATTEMPTS as usize);
        let u = upgrades.get("pi-1").unwrap();
        assert_eq!(u.state, UpgradeState::Failed);
        assert!(u.error.is_some());
    }

    #[rstest]
    #[case(UpgradeOutcome::UpToDate, "", None)]
    #[case(UpgradeOutcome::Failed, "no such release", Some(UpgradeState::Failed))]
    #[case(UpgradeOutcome::Updated, "", Some(UpgradeState::Requested))]
    fn reports_settle_the_request(
        #[case] outcome: UpgradeOutcome,
        #[case] error: &str,
        #[case] state: Option<UpgradeState>,
    ) {
        let (_, upgrades) = fixture();
        upgrades
            .request("pi-1", Some("2.2.0"), None, "ben", Utc::now())
            .unwrap();
        upgrades.reported("pi-1", "2.3.0", outcome, error);
        let got = upgrades.get("pi-1");
        assert_eq!(got.as_ref().map(|u| u.state), state);
        if !error.is_empty() {
            assert_eq!(got.and_then(|u| u.error).as_deref(), Some(error));
        }
    }

    #[test]
    fn a_report_for_another_version_is_ignored() {
        let (_, upgrades) = fixture();
        upgrades
            .request("pi-1", Some("2.2.0"), None, "ben", Utc::now())
            .unwrap();
        assert_eq!(
            upgrades.reported("pi-1", "2.1.0", UpgradeOutcome::Failed, "old"),
            None
        );
        assert_eq!(upgrades.get("pi-1").unwrap().state, UpgradeState::Requested);
    }
}
