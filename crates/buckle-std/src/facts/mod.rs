//! Fact providers. See the design document section 05 for the catalogue.
//!
//! Every provider reads the host through [`buckle_core::system::System`] only
//! (files with `read`, commands with `run`, the metadata services with
//! `http`), so tests run against `FakeSystem`. Providers are cheap, tolerate
//! missing tools and fields, and return `null` for what they cannot find; the
//! engine records a provider that returns an error as `null` too.

pub mod cloud;
pub mod docker;
pub mod hardware;
pub mod net;
pub mod nomad;
pub mod os;
pub mod script;
pub mod systemd;
pub mod tailscale;
pub mod users;

use buckle_core::system::Command;
use buckle_core::task::Cx;
use std::time::Duration;

/// How long a probe command may take before it is abandoned.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `program args...` and returns its stdout when it exits 0. A missing
/// program, a failure or a timeout is `None`: facts are best effort.
pub(crate) async fn probe(cx: &Cx, program: &str, args: &[&str]) -> Option<String> {
    let out = cx
        .system
        .run(
            Command::new(program)
                .args(args.iter().copied())
                .timeout(PROBE_TIMEOUT),
        )
        .await
        .ok()?;
    out.success().then(|| out.stdout_str())
}

/// Like [`probe`] but parses the output as JSON.
pub(crate) async fn probe_json(cx: &Cx, program: &str, args: &[&str]) -> Option<serde_json::Value> {
    serde_json::from_str(&probe(cx, program, args).await?).ok()
}

/// Reads a UTF-8 text file through the system; `None` when missing or unreadable.
pub(crate) async fn read_text(cx: &Cx, path: &str) -> Option<String> {
    cx.system
        .read(std::path::Path::new(path))
        .await
        .ok()
        .flatten()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `Some(s)` becomes a JSON string, `None` becomes `null`.
pub(crate) fn opt_str(s: Option<impl Into<String>>) -> serde_json::Value {
    s.map(|s| serde_json::Value::String(s.into()))
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::facts::{Facts, collect, registry};
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;

    #[tokio::test]
    async fn probe_returns_stdout_only_on_success() {
        let sys = FakeSystem::new()
            .on_command_ok("uname", &["-r"], "6.1.0-18-amd64\n")
            .on_command_fail("uname", &["-z"], 1, "unknown option");
        let cx = cx(&sys);
        assert_eq!(
            probe(&cx, "uname", &["-r"]).await.as_deref(),
            Some("6.1.0-18-amd64\n")
        );
        assert_eq!(probe(&cx, "uname", &["-z"]).await, None);
        assert_eq!(
            probe(&cx, "nope", &[]).await,
            None,
            "unscripted commands are tolerated"
        );
    }

    #[test]
    fn every_provider_is_registered_under_its_documented_name() {
        for name in [
            "os",
            "hardware",
            "net",
            "tailscale",
            "cloud",
            "users",
            "systemd",
            "nomad",
            "docker",
            "script",
        ] {
            assert!(registry().contains_key(name), "{name} is not registered");
        }
    }

    fn debian_host() -> FakeSystem {
        FakeSystem::new()
            .with_hostname("web-1")
            .with_file(
                "/etc/os-release",
                super::os::tests::DEBIAN_12,
                FilePerms::default(),
            )
            .with_file(
                "/proc/meminfo",
                super::hardware::tests::MEMINFO,
                FilePerms::default(),
            )
            .with_file(
                "/proc/cpuinfo",
                super::hardware::tests::CPUINFO_4,
                FilePerms::default(),
            )
            .on_command_ok("uname", &["-r"], "6.1.0-18-amd64\n")
            .on_command_ok("uname", &["-m"], "x86_64\n")
            .on_command_ok("df", &["-P"], super::hardware::tests::DF_T)
            .on_command_ok("hostname", &["-f"], "web-1.example.com\n")
            .on_command_ok("ip", &["-j", "addr"], super::net::tests::IP_ADDR)
            .on_command_ok("ip", &["-j", "route"], super::net::tests::IP_ROUTE)
            .on_command_ok(
                "systemctl",
                &["list-units"],
                super::systemd::tests::LIST_UNITS,
            )
            .on_command_ok("systemctl", &["--version"], super::systemd::tests::VERSION)
            .with_program("tailscale")
            .on_command_ok("tailscale", &["status"], super::tailscale::tests::STATUS)
            .with_user(buckle_core::system::UserInfo {
                name: "root".into(),
                uid: 0,
                gid: 0,
                home: "/root".into(),
                shell: "/bin/bash".into(),
                gecos: String::new(),
            })
            .on_http_ok(
                "http://169.254.169.254/opc/v2/instance/",
                super::cloud::tests::OCI,
            )
    }

    #[tokio::test]
    async fn collect_over_the_registry_yields_the_documented_tree() {
        let sys = debian_host();
        let cx = cx(&sys);
        let mut facts = Facts::new();
        let errors = collect(&cx, &mut facts, None, true).await;
        let tree = &facts.tree;

        assert_eq!(tree["os"]["family"], "debian");
        assert_eq!(tree["os"]["codename"], "bookworm");
        assert_eq!(tree["os"]["kernel"], "6.1.0-18-amd64");
        assert_eq!(tree["hardware"]["arch"], "x86_64");
        assert_eq!(tree["hardware"]["cpus"], 4);
        assert_eq!(tree["hardware"]["mem"]["total_mb"], 3931);
        assert_eq!(tree["hardware"]["disk"]["root"]["mount"], "/");
        assert_eq!(tree["net"]["hostname"], "web-1");
        assert_eq!(tree["net"]["fqdn"], "web-1.example.com");
        assert_eq!(tree["net"]["default_route"]["gateway"], "10.0.0.1");
        assert_eq!(tree["tailscale"]["hostname"], "web-1.tail1234.ts.net");
        assert_eq!(tree["tailscale"]["online"], true);
        assert_eq!(tree["cloud"]["provider"], "oci");
        assert_eq!(tree["cloud"]["region"], "uk-london-1");
        assert_eq!(tree["users"]["accounts"][0]["name"], "root");
        assert_eq!(tree["systemd"]["version"], 252);
        // Not installed: present in the tree, but null.
        assert!(tree["nomad"].is_null());
        assert!(tree["docker"].is_null());
        // The error list only has providers that returned errors; none here.
        assert!(errors.is_empty(), "{errors:?}");
        // Every registered provider has a slot in the tree.
        for name in registry().keys() {
            assert!(
                tree.get(*name).is_some(),
                "{name} is missing from the facts tree"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_provider_yields_null_and_does_not_abort_the_others() {
        // `systemctl list-units` fails (a container without systemd).
        let sys = FakeSystem::new()
            .with_file(
                "/etc/os-release",
                super::os::tests::UBUNTU_2404,
                FilePerms::default(),
            )
            .on_command_ok("uname", &["-r"], "6.8.0-45-generic\n")
            .on_command_fail(
                "systemctl",
                &["list-units"],
                1,
                "System has not been booted with systemd",
            );
        let cx = cx(&sys);
        let mut facts = Facts::new();
        let errors = collect(&cx, &mut facts, None, true).await;
        assert!(facts.tree["systemd"].is_null());
        assert_eq!(facts.tree["os"]["id"], "ubuntu");
        assert!(errors.iter().any(|e| e.provider == "systemd"), "{errors:?}");
        assert!(facts.collected_at.contains_key("systemd"));
    }
}
