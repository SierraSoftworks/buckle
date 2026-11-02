//! The `systemd` fact: loaded units and the systemd version.

use super::probe;
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Lists systemd units.
///
/// Keys: `version` (the systemd release number, from `systemctl --version`),
/// `units` (every loaded unit as `{name, load, active, sub}` from `systemctl
/// list-units --all`), and the flat lists `active` and `failed` holding unit
/// names, because selectors cannot look inside a list of objects
/// (`host.facts.systemd.active contains "nomad.service"`). `null` when
/// systemctl is not installed; a host where it fails (a container without
/// systemd) records `null` with a warning. Refreshed every minute.
pub struct SystemdFacts;

/// Parses `systemctl list-units --all --plain --no-legend --no-pager`.
pub(crate) fn parse_units(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace().peekable();
            // Failed or missing units may be prefixed with a bullet.
            if parts
                .peek()
                .is_some_and(|p| matches!(*p, "●" | "○" | "*" | "×"))
            {
                parts.next();
            }
            let name = parts.next()?;
            let (load, active, sub) = (parts.next()?, parts.next()?, parts.next()?);
            name.contains('.')
                .then(|| json!({"name": name, "load": load, "active": active, "sub": sub}))
        })
        .collect()
}

/// The release number from `systemctl --version`'s first line (`systemd 252 (252.22-1~deb12u1)`).
pub(crate) fn parse_version(text: &str) -> Value {
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .map(|v| {
            v.parse::<u64>()
                .map(Value::from)
                .unwrap_or_else(|_| json!(v))
        })
        .unwrap_or(Value::Null)
}

#[async_trait]
impl FactProvider for SystemdFacts {
    const NAME: &'static str = "systemd";
    const TTL: Duration = Duration::from_secs(60);
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "systemd version and units (with flat active and failed name lists).";

    #[tracing::instrument(name = "facts.systemd", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        if cx.system.which("systemctl").await?.is_none() {
            return Ok(Value::Null);
        }
        let units_out = probe(
            cx,
            "systemctl",
            &["list-units", "--all", "--plain", "--no-legend", "--no-pager"],
        )
        .await
        .ok_or_else(|| {
            buckle_core::error::user(
                "`systemctl list-units` failed.",
                &["systemd may not be the init system on this host (for example inside a container)."],
            )
        })?;
        let units = parse_units(&units_out);
        let names = |state: &str| -> Vec<Value> {
            units
                .iter()
                .filter(|u| u["active"] == state)
                .map(|u| u["name"].clone())
                .collect()
        };
        let version = probe(cx, "systemctl", &["--version"])
            .await
            .map(|v| parse_version(&v))
            .unwrap_or(Value::Null);
        Ok(json!({
            "version": version,
            "active": names("active"),
            "failed": names("failed"),
            "units": units,
        }))
    }
}

buckle_core::register_facts!(SystemdFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::testing::FakeSystem;

    pub const LIST_UNITS: &str = "\
  proc-sys-fs-binfmt_misc.automount loaded    active   waiting Arbitrary Executable File Formats File System Automount Point
  sys-devices-virtual-net-tailscale0.device loaded active plugged /sys/devices/virtual/net/tailscale0
  cron.service                      loaded    active   running Regular background program processing daemon
  nomad.service                     loaded    active   running Nomad
  ssh.service                       loaded    active   running OpenBSD Secure Shell server
● rustic-backup.service             loaded    failed   failed  Nightly backup
  systemd-networkd.service          loaded    inactive dead    Network Configuration
● ghost.service                     not-found inactive dead    ghost.service
  multi-user.target                 loaded    active   active  Multi-User System
";

    pub const VERSION: &str = "\
systemd 252 (252.22-1~deb12u1)
+PAM +AUDIT +SELINUX +APPARMOR +IMA +SMACK +SECCOMP +GCRYPT -GNUTLS +OPENSSL +ACL +BLKID +CURL +ELFUTILS +FIDO2 +IDN2 -IDN +IPTC +KMOD +LIBCRYPTSETUP +LIBFDISK +PCRE2 -PWQUALITY +P11KIT +QRENCODE +TPM2 +BZIP2 +LZ4 +XZ +ZLIB +ZSTD -BPF_FRAMEWORK -XKBCOMMON +UTMP +SYSVINIT default-hierarchy=unified
";

    #[test]
    fn units_parse_including_bulleted_failures() {
        let units = parse_units(LIST_UNITS);
        assert_eq!(units.len(), 9);
        let nomad = units.iter().find(|u| u["name"] == "nomad.service").unwrap();
        assert_eq!(
            nomad,
            &json!({"name": "nomad.service", "load": "loaded", "active": "active", "sub": "running"})
        );
        let failed = units
            .iter()
            .find(|u| u["name"] == "rustic-backup.service")
            .unwrap();
        assert_eq!(failed["active"], "failed");
        let ghost = units.iter().find(|u| u["name"] == "ghost.service").unwrap();
        assert_eq!(ghost["load"], "not-found");
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version(VERSION), json!(252));
        assert_eq!(
            parse_version("systemd v255.4-1ubuntu8\n"),
            json!("v255.4-1ubuntu8")
        );
        assert!(parse_version("").is_null());
    }

    #[tokio::test]
    async fn collects_units_and_flat_lists() {
        let sys = FakeSystem::new()
            .on_command_ok("systemctl", &["list-units"], LIST_UNITS)
            .on_command_ok("systemctl", &["--version"], VERSION);
        let f = SystemdFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["version"], 252);
        let active = f["active"].as_array().unwrap();
        assert!(active.contains(&json!("nomad.service")));
        assert!(!active.contains(&json!("systemd-networkd.service")));
        assert_eq!(f["failed"], json!(["rustic-backup.service"]));
        assert_eq!(f["units"].as_array().unwrap().len(), 9);
    }

    #[tokio::test]
    async fn null_without_systemctl_and_error_when_it_fails() {
        assert!(
            SystemdFacts
                .collect(&cx(&FakeSystem::new()))
                .await
                .unwrap()
                .is_null()
        );
        let sys = FakeSystem::new().on_command_fail(
            "systemctl",
            &["list-units"],
            1,
            "System has not been booted with systemd as init system",
        );
        assert!(SystemdFacts.collect(&cx(&sys)).await.is_err());
    }
}
