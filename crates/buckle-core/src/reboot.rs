//! Reboots asked for by a run.
//!
//! A task (normally the `system.reboot` handler) never reboots the host
//! itself: it returns a [`RebootRequest`] in its [`crate::task::Applied`], the
//! engine collects the requests of the run into
//! [`crate::engine::ApplyOutcome::reboot`], and whoever drove the run (the
//! agent, or `buckle apply`) reboots once the run has finished and been
//! reported. A host reboots at most once per run.

use crate::system::{Command, System};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Where Linux exposes the identity of the current boot.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// A reboot a run asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebootRequest {
    /// How long to wait after the run has been reported before rebooting.
    #[serde(with = "humantime_serde")]
    pub delay: Duration,
    /// How long a rollout waits for the host to come back before failing it.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// The steps that asked for it: `module/kind[key]`, or `owner:handler`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_by: Vec<String>,
    /// The boot the run happened in, set by the agent before it reports the
    /// run, so a reconnect after the reboot can be told from one before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
}

impl RebootRequest {
    /// A request with no requester yet.
    pub fn new(delay: Duration, timeout: Duration) -> Self {
        Self {
            delay,
            timeout,
            requested_by: Vec::new(),
            boot_id: None,
        }
    }

    /// Folds another request of the same run into this one: the host reboots
    /// once, after the longest delay, and is waited for the longest timeout.
    pub fn merge(&mut self, other: RebootRequest) {
        self.delay = self.delay.max(other.delay);
        self.timeout = self.timeout.max(other.timeout);
        for by in other.requested_by {
            if !self.requested_by.contains(&by) {
                self.requested_by.push(by);
            }
        }
    }
}

/// The identity of the current boot, when the platform exposes one.
pub async fn boot_id(system: &dyn System) -> Option<String> {
    let bytes = system.read(Path::new(BOOT_ID_PATH)).await.ok()??;
    let id = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!id.is_empty()).then_some(id)
}

/// The command that reboots this host: `systemctl reboot` under systemd, else
/// `shutdown -r now`. `None` when neither is available.
pub async fn command(system: &dyn System) -> crate::Result<Option<Command>> {
    let systemd = system
        .stat(Path::new("/run/systemd/system"))
        .await?
        .is_some();
    if systemd && system.which("systemctl").await?.is_some() {
        return Ok(Some(Command::new("systemctl").arg("reboot")));
    }
    if system.which("shutdown").await?.is_some() {
        return Ok(Some(Command::new("shutdown").arg("-r").arg("now")));
    }
    Ok(None)
}

/// Reboots the host now. Returns once the reboot has been scheduled (the
/// process is normally stopped shortly after).
pub async fn reboot(system: &dyn System) -> crate::Result<()> {
    let Some(command) = command(system).await? else {
        return Err(crate::error::user(
            "This host cannot be rebooted: neither `systemctl` (under systemd) nor `shutdown` is available.",
            &["Reboot the host by hand."],
        ));
    };
    let line = command.display_line();
    let out = system.run(command).await?;
    if out.success() {
        Ok(())
    } else {
        Err(crate::error::user(
            format!(
                "`{line}` failed (exit {}): {}",
                out.code
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                out.stderr_str().trim()
            ),
            &["Check that the agent runs as root, then reboot the host by hand."],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FilePerms;
    use crate::testing::FakeSystem;
    use rstest::rstest;

    fn request(delay: u64, timeout: u64, by: &[&str]) -> RebootRequest {
        RebootRequest {
            requested_by: by.iter().map(|s| (*s).to_owned()).collect(),
            ..RebootRequest::new(Duration::from_secs(delay), Duration::from_secs(timeout))
        }
    }

    #[test]
    fn merging_keeps_the_longest_wait_and_every_requester_once() {
        let mut a = request(5, 600, &["pi:reboot"]);
        a.merge(request(30, 60, &["kernel:reboot", "pi:reboot"]));
        assert_eq!(a.delay, Duration::from_secs(30));
        assert_eq!(a.timeout, Duration::from_secs(600));
        assert_eq!(a.requested_by, vec!["pi:reboot", "kernel:reboot"]);
    }

    #[tokio::test]
    async fn boot_id_is_read_trimmed_and_absent_without_procfs() {
        let sys = FakeSystem::new().with_file(BOOT_ID_PATH, "abc-123\n", FilePerms::mode(0o444));
        assert_eq!(boot_id(&sys).await.as_deref(), Some("abc-123"));
        assert_eq!(boot_id(&FakeSystem::new()).await, None);
    }

    #[rstest]
    #[case::systemd(true, &["systemctl", "shutdown"], Some("systemctl reboot"))]
    #[case::sysv(false, &["systemctl", "shutdown"], Some("shutdown -r now"))]
    #[case::nothing(false, &[], None)]
    #[tokio::test]
    async fn the_reboot_command_follows_the_init_system(
        #[case] systemd: bool,
        #[case] programs: &[&str],
        #[case] expected: Option<&str>,
    ) {
        let mut sys = FakeSystem::new();
        if systemd {
            sys = sys.with_dir("/run/systemd/system", FilePerms::mode(0o755));
        }
        for p in programs {
            sys = sys.with_program(p);
        }
        let command = command(&sys).await.unwrap();
        assert_eq!(command.map(|c| c.display_line()).as_deref(), expected);
    }

    #[tokio::test]
    async fn a_failing_reboot_is_an_error() {
        let sys = FakeSystem::new()
            .with_dir("/run/systemd/system", FilePerms::mode(0o755))
            .with_program("systemctl")
            .on_command_fail("systemctl", &["reboot"], 1, "Access denied");
        let err = reboot(&sys).await.unwrap_err();
        assert!(err.description().contains("Access denied"));
    }
}
