//! The `os` fact: distribution family, id, codename, version and kernel.

use super::{opt_str, probe, read_text};
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platform, Platforms, Result};
use std::collections::BTreeMap;
use std::time::Duration;

/// Describes the operating system.
///
/// Keys: `family` (`debian`, `rhel`, `alpine`, `arch`, `macos`, `windows` or
/// `unknown`, derived from `ID_LIKE` then `ID` in `/etc/os-release`), `id`
/// (`debian`, `ubuntu`, `raspbian`, ...), `codename` (`VERSION_CODENAME`),
/// `version` (`VERSION_ID`), `pretty_name` and `kernel` (`uname -r`). Keys that
/// cannot be determined are `null`. Refreshed hourly.
pub struct OsFacts;

/// Parses `/etc/os-release` into key/value pairs (quotes removed).
pub(crate) fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

/// The distribution family for an `ID` and `ID_LIKE` pair.
pub(crate) fn family_of(id: &str, id_like: &str) -> &'static str {
    for token in std::iter::once(id).chain(id_like.split_whitespace()) {
        match token.to_ascii_lowercase().as_str() {
            "debian" | "ubuntu" | "raspbian" | "linuxmint" | "pop" | "kali" => return "debian",
            "rhel" | "fedora" | "centos" | "rocky" | "almalinux" | "ol" | "amzn" => return "rhel",
            "alpine" => return "alpine",
            "arch" | "archarm" | "manjaro" => return "arch",
            _ => {}
        }
    }
    "unknown"
}

#[async_trait]
impl FactProvider for OsFacts {
    const NAME: &'static str = "os";
    const TTL: Duration = Duration::from_secs(3600);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Operating system: family, id, codename, version, pretty_name and kernel.";

    #[tracing::instrument(name = "facts.os", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<serde_json::Value> {
        let kernel = probe(cx, "uname", &["-r"])
            .await
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        match cx.platform() {
            Platform::Linux => {
                let text = match read_text(cx, "/etc/os-release").await {
                    Some(t) => Some(t),
                    None => read_text(cx, "/usr/lib/os-release").await,
                };
                let release = text.as_deref().map(parse_os_release).unwrap_or_default();
                let get = |k: &str| release.get(k).cloned().filter(|v| !v.is_empty());
                let id = get("ID").unwrap_or_default();
                let family = family_of(&id, &get("ID_LIKE").unwrap_or_default());
                Ok(serde_json::json!({
                    "family": family,
                    "id": opt_str(get("ID")),
                    "codename": opt_str(get("VERSION_CODENAME")),
                    "version": opt_str(get("VERSION_ID")),
                    "pretty_name": opt_str(get("PRETTY_NAME")),
                    "kernel": opt_str(kernel),
                }))
            }
            Platform::MacOs => {
                let version = probe(cx, "sw_vers", &["-productVersion"])
                    .await
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty());
                Ok(serde_json::json!({
                    "family": "macos",
                    "id": "macos",
                    "codename": null,
                    "pretty_name": version.as_ref().map(|v| format!("macOS {v}")),
                    "version": opt_str(version),
                    "kernel": opt_str(kernel),
                }))
            }
            Platform::Windows => Ok(serde_json::json!({
                "family": "windows",
                "id": "windows",
                "codename": null,
                "version": null,
                "pretty_name": "Windows",
                "kernel": null,
            })),
        }
    }
}

buckle_core::register_facts!(OsFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;

    pub const DEBIAN_12: &str = "\
PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"
NAME=\"Debian GNU/Linux\"
VERSION_ID=\"12\"
VERSION=\"12 (bookworm)\"
VERSION_CODENAME=bookworm
ID=debian
HOME_URL=\"https://www.debian.org/\"
SUPPORT_URL=\"https://www.debian.org/support\"
BUG_REPORT_URL=\"https://bugs.debian.org/\"
";

    pub const UBUNTU_2404: &str = "\
PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"
NAME=\"Ubuntu\"
VERSION_ID=\"24.04\"
VERSION=\"24.04.1 LTS (Noble Numbat)\"
VERSION_CODENAME=noble
ID=ubuntu
ID_LIKE=debian
HOME_URL=\"https://www.ubuntu.com/\"
SUPPORT_URL=\"https://help.ubuntu.com/\"
BUG_REPORT_URL=\"https://bugs.launchpad.net/ubuntu/\"
PRIVACY_POLICY_URL=\"https://www.ubuntu.com/legal/terms-and-policies/privacy-policy\"
UBUNTU_CODENAME=noble
LOGO=ubuntu-logo
";

    pub const RASPBERRY_PI_OS_11: &str = "\
PRETTY_NAME=\"Raspbian GNU/Linux 11 (bullseye)\"
NAME=\"Raspbian GNU/Linux\"
VERSION_ID=\"11\"
VERSION=\"11 (bullseye)\"
VERSION_CODENAME=bullseye
ID=raspbian
ID_LIKE=debian
HOME_URL=\"http://www.raspbian.org/\"
SUPPORT_URL=\"http://www.raspbian.org/RaspbianForums\"
BUG_REPORT_URL=\"http://www.raspbian.org/RaspbianBugs\"
";

    const ROCKY_9: &str = "\
NAME=\"Rocky Linux\"
VERSION=\"9.4 (Blue Onyx)\"
ID=\"rocky\"
ID_LIKE=\"rhel centos fedora\"
VERSION_ID=\"9.4\"
PLATFORM_ID=\"platform:el9\"
PRETTY_NAME=\"Rocky Linux 9.4 (Blue Onyx)\"
";

    const ALPINE: &str = "\
NAME=\"Alpine Linux\"
ID=alpine
VERSION_ID=3.20.3
PRETTY_NAME=\"Alpine Linux v3.20\"
HOME_URL=\"https://alpinelinux.org/\"
";

    #[rstest]
    #[case::debian12(
        DEBIAN_12,
        "debian",
        "debian",
        Some("bookworm"),
        "12",
        "Debian GNU/Linux 12 (bookworm)"
    )]
    #[case::ubuntu2404(
        UBUNTU_2404,
        "debian",
        "ubuntu",
        Some("noble"),
        "24.04",
        "Ubuntu 24.04.1 LTS"
    )]
    #[case::raspberry_pi_os(
        RASPBERRY_PI_OS_11,
        "debian",
        "raspbian",
        Some("bullseye"),
        "11",
        "Raspbian GNU/Linux 11 (bullseye)"
    )]
    #[case::rocky(ROCKY_9, "rhel", "rocky", None, "9.4", "Rocky Linux 9.4 (Blue Onyx)")]
    #[case::alpine(ALPINE, "alpine", "alpine", None, "3.20.3", "Alpine Linux v3.20")]
    #[tokio::test]
    async fn linux_distributions_are_recognised(
        #[case] release: &str,
        #[case] family: &str,
        #[case] id: &str,
        #[case] codename: Option<&str>,
        #[case] version: &str,
        #[case] pretty: &str,
    ) {
        let sys = FakeSystem::new()
            .with_file("/etc/os-release", release, FilePerms::default())
            .on_command_ok("uname", &["-r"], "6.1.0-18-amd64\n");
        let cx = cx(&sys);
        let facts = OsFacts.collect(&cx).await.unwrap();
        assert_eq!(facts["family"], family);
        assert_eq!(facts["id"], id);
        assert_eq!(facts["version"], version);
        assert_eq!(facts["pretty_name"], pretty);
        assert_eq!(facts["kernel"], "6.1.0-18-amd64");
        match codename {
            Some(c) => assert_eq!(facts["codename"], c),
            None => assert!(facts["codename"].is_null()),
        }
    }

    #[tokio::test]
    async fn falls_back_to_usr_lib_and_tolerates_nothing_at_all() {
        let sys =
            FakeSystem::new().with_file("/usr/lib/os-release", DEBIAN_12, FilePerms::default());
        let facts = OsFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(facts["family"], "debian");
        assert!(facts["kernel"].is_null(), "uname is not scripted");

        let bare = OsFacts.collect(&cx(&FakeSystem::new())).await.unwrap();
        assert_eq!(bare["family"], "unknown");
        assert!(bare["id"].is_null());
    }

    #[tokio::test]
    async fn macos_and_windows_use_platform_defaults() {
        let mac = FakeSystem::new()
            .with_platform(Platform::MacOs)
            .on_command_ok("sw_vers", &["-productVersion"], "14.5\n")
            .on_command_ok("uname", &["-r"], "23.5.0\n");
        let facts = OsFacts.collect(&cx(&mac)).await.unwrap();
        assert_eq!(facts["family"], "macos");
        assert_eq!(facts["version"], "14.5");
        assert_eq!(facts["pretty_name"], "macOS 14.5");

        let win = FakeSystem::new().with_platform(Platform::Windows);
        assert_eq!(
            OsFacts.collect(&cx(&win)).await.unwrap()["family"],
            "windows"
        );
    }

    #[test]
    fn os_release_parsing_strips_quotes_and_comments() {
        let r = parse_os_release(
            "# comment\nID=\"debian\"\nNAME='Debian GNU/Linux'\nBAD LINE\nEMPTY=\n",
        );
        assert_eq!(r["ID"], "debian");
        assert_eq!(r["NAME"], "Debian GNU/Linux");
        assert_eq!(r["EMPTY"], "");
        assert!(!r.contains_key("BAD LINE"));
    }
}
