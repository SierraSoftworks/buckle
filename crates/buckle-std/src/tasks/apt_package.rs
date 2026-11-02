//! `apt.package`: install, upgrade or remove Debian packages.

use super::apt_common::{PkgState, apt_get, query_package, query_policy, version_matches};
use super::sys_exec::{Ledger, run_checked, state_dir};
use async_trait::async_trait;
use buckle_core::Platforms;
use buckle_core::Result;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FilePerms, HttpRequest};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Installs, upgrades or removes Debian/Ubuntu packages with `apt-get`.
///
/// Give `name` for one package or `names` for several. `state` is `present`
/// (default: installed, at `version` when given), `latest` (upgraded whenever
/// the apt cache offers a newer candidate) or `absent`. `version` accepts `*`
/// wildcards (`1.22.*`) and applies to a single package with `state: present`.
/// `deb` installs a local `.deb` downloaded from a URL (it needs `name`, the
/// package the file provides, so the task can tell whether it is installed).
/// `update_cache` runs `apt-get update` before installing, and
/// `install_recommends: false` passes `--no-install-recommends`.
///
/// Plans compare `dpkg-query` status (and `apt-cache policy` for `latest`);
/// they cannot see packages that a repository added earlier in the same run
/// will provide, so a package that is not installed is always planned as an
/// install. Held packages are reported as blocked rather than changed.
///
/// Removal removes only the packages this resource installed itself:
/// packages that were already on the host when Buckle first managed them are
/// left alone. Plans flag the removal with a caution.
pub struct AptPackage;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// A single package name.
    #[serde(default)]
    pub name: Option<String>,
    /// Several package names, managed as one resource.
    #[serde(default)]
    pub names: Vec<String>,
    /// `present` (default), `latest` or `absent`.
    #[serde(default)]
    pub state: State,
    /// Exact version to install (`1.22.1-9`), or a wildcard such as `1.22.*`.
    #[serde(default)]
    pub version: Option<String>,
    /// URL of a `.deb` to download and install; requires `name`.
    #[serde(default)]
    pub deb: Option<String>,
    /// Run `apt-get update` before installing.
    #[serde(default)]
    pub update_cache: bool,
    /// Install recommended packages too (apt's default).
    #[serde(default = "default_true")]
    pub install_recommends: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Present,
    Absent,
    Latest,
}

impl Spec {
    /// The packages this resource manages, validated.
    fn packages(&self) -> Result<Vec<String>> {
        let mut pkgs: Vec<String> = self.name.iter().cloned().collect();
        pkgs.extend(self.names.iter().cloned());
        pkgs.retain(|p| !p.trim().is_empty());
        let invalid = |msg: &str| {
            buckle_core::error::user(
                format!("apt.package: {msg}"),
                &["See `buckle catalog apt.package` for the fields this task accepts."],
            )
        };
        if pkgs.is_empty() {
            return Err(invalid("set `name` or `names`."));
        }
        if self.version.is_some() && (pkgs.len() != 1 || self.state != State::Present) {
            return Err(invalid(
                "`version` needs exactly one package and `state: present`.",
            ));
        }
        if self.deb.is_some() && (pkgs.len() != 1 || self.state != State::Present) {
            return Err(invalid(
                "`deb` needs exactly one `name` (the package the file provides) and `state: present`.",
            ));
        }
        Ok(pkgs)
    }
}

#[async_trait]
impl Task for AptPackage {
    type Spec = Spec;
    const KIND: &'static str = "apt.package";
    const LOCKS: &'static [&'static str] = &["apt"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_CAUTION: bool = true;
    const DESCRIPTION: &'static str = "Install, upgrade or remove Debian packages (present, latest, absent; pinned versions; .deb URLs).";

    fn key(spec: &Spec) -> String {
        let mut names: Vec<&str> = spec.name.iter().map(String::as_str).collect();
        names.extend(spec.names.iter().map(String::as_str));
        names.join(",")
    }

    fn describe(spec: &Spec) -> String {
        let key = Self::key(spec);
        match (spec.state, &spec.version) {
            (State::Absent, _) => format!("{key} absent"),
            (State::Latest, _) => format!("{key} latest"),
            (State::Present, Some(v)) => format!("{key}={v} present"),
            (State::Present, None) => format!("{key} present"),
        }
    }

    #[tracing::instrument(name = "task.apt.package.plan", skip_all, fields(packages = %Self::key(spec)))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let pkgs = spec.packages()?;
        let mut actions: Vec<String> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        let mut upgrades = false;
        for p in &pkgs {
            let status = query_package(cx, p).await?;
            match (spec.state, &status) {
                (State::Absent, PkgState::Installed { held: true, .. }) => {
                    blocked.push(format!("{p} is held and cannot be removed"));
                }
                (State::Absent, PkgState::Installed { version, .. }) => {
                    actions.push(format!("remove {p} {version}"));
                }
                (State::Absent, PkgState::Broken { state, .. }) => {
                    actions.push(format!("remove {p} (dpkg state: {state})"));
                }
                (State::Absent, _) => {}
                (State::Present | State::Latest, PkgState::Installed { version, held }) => {
                    let target = if let Some(want) = &spec.version {
                        (!version_matches(version, want)).then(|| want.clone())
                    } else if spec.state == State::Latest {
                        query_policy(cx, p)
                            .await?
                            .candidate
                            .filter(|c| c != version)
                    } else {
                        None
                    };
                    if let Some(target) = target {
                        if *held {
                            blocked
                                .push(format!("{p} is held at {version}; cannot move to {target}"));
                        } else {
                            upgrades = true;
                            actions.push(format!("change {p} {version} -> {target}"));
                        }
                    }
                }
                (State::Present | State::Latest, PkgState::Broken { state, .. }) => {
                    upgrades = true;
                    actions.push(format!("repair {p} (dpkg state: {state})"));
                }
                (State::Present | State::Latest, _) => {
                    actions.push(match &spec.version {
                        Some(v) => format!("install {p}={v}"),
                        None => format!("install {p}"),
                    });
                }
            }
        }
        if !blocked.is_empty() {
            return Ok(Planned::blocked(blocked.join("; ")));
        }
        if actions.is_empty() {
            return Ok(Planned::NoOp);
        }
        let summary = actions.join(", ");
        Ok(Planned::change(match spec.state {
            State::Absent => Diff::Delete { summary },
            _ if upgrades => Diff::Opaque { summary },
            _ => Diff::Create {
                summary,
                preview: None,
            },
        }))
    }

    #[tracing::instrument(name = "task.apt.package.apply", skip_all, fields(packages = %Self::key(spec)))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let pkgs = spec.packages()?;
        let mut ledger = Ledger::open(cx, "apt.package").await?;

        if spec.state == State::Absent {
            let mut installed = Vec::new();
            for p in &pkgs {
                if !matches!(
                    query_package(cx, p).await?,
                    PkgState::NotInstalled | PkgState::ConfigFiles { .. }
                ) {
                    installed.push(p.clone());
                }
            }
            if installed.is_empty() {
                return Ok(Applied::unchanged(format!(
                    "{} is already absent",
                    pkgs.join(", ")
                )));
            }
            run_checked(cx, apt_get("remove", true).args(installed.clone())).await?;
            for p in &installed {
                ledger.remove(p);
            }
            ledger.save(cx).await?;
            return Ok(Applied::changed(format!(
                "removed {}",
                installed.join(", ")
            )));
        }

        if spec.update_cache {
            run_checked(cx, apt_get("update", false)).await?;
        }

        let mut was_installed = Vec::new();
        for p in &pkgs {
            was_installed.push(query_package(cx, p).await?.is_installed());
        }

        let mut install = apt_get("install", true);
        if !spec.install_recommends {
            install = install.arg("--no-install-recommends");
        }
        let downloaded = match &spec.deb {
            Some(url) => {
                let path = download_deb(cx, url).await?;
                install = install.arg(path.to_string_lossy().into_owned());
                Some(path)
            }
            None => {
                for p in &pkgs {
                    install = install.arg(match &spec.version {
                        Some(v) => format!("{p}={v}"),
                        None => p.clone(),
                    });
                }
                None
            }
        };
        let result = run_checked(cx, install).await;
        if let Some(path) = downloaded {
            let _ = cx.system.remove(&path, false).await;
        }
        result?;

        let mut applied = Applied::changed(format!("installed {}", pkgs.join(", ")));
        for (p, existed) in pkgs.iter().zip(was_installed) {
            if !existed || ledger.contains(p) {
                ledger.insert(p.clone(), "installed");
                applied = applied.with_artefact(Artefact::Package {
                    manager: "apt".into(),
                    name: p.clone(),
                });
            }
        }
        ledger.save(cx).await?;
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let names = installed_by_buckle(record);
        if names.is_empty() {
            format!(
                "leave {} installed (Buckle did not install it)",
                record.key.key
            )
        } else {
            format!("apt-get remove {}", names.join(" "))
        }
    }

    #[tracing::instrument(name = "task.apt.package.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let names = installed_by_buckle(record);
        let mut ledger = Ledger::open(cx, "apt.package").await?;
        if names.is_empty() {
            return Ok(Applied::skipped(
                "Buckle did not install any package for this resource; nothing was removed",
            ));
        }
        let mut present = Vec::new();
        for n in &names {
            if query_package(cx, n).await?.is_installed() {
                present.push(n.clone());
            }
        }
        for n in &names {
            ledger.remove(n);
        }
        ledger.save(cx).await?;
        if present.is_empty() {
            return Ok(Applied::unchanged(format!(
                "{} was already removed",
                names.join(", ")
            )));
        }
        run_checked(cx, apt_get("remove", true).args(present.clone())).await?;
        Ok(Applied::changed(format!("removed {}", present.join(", "))))
    }
}

fn installed_by_buckle(record: &ResourceRecord) -> Vec<String> {
    record
        .artefacts
        .iter()
        .filter_map(|a| match a {
            Artefact::Package { manager, name } if manager == "apt" => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Downloads a `.deb` into Buckle's state directory and returns its path.
async fn download_deb(cx: &Cx, url: &str) -> Result<PathBuf> {
    let file = url
        .split(['?', '#'])
        .next()
        .and_then(|u| u.rsplit('/').next())
        .filter(|f| !f.is_empty())
        .unwrap_or("package.deb");
    let file: String = file
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '~') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = if file.ends_with(".deb") {
        file
    } else {
        format!("{file}.deb")
    };
    let response = cx
        .system
        .http(HttpRequest::get(url).timeout(Duration::from_secs(600)))
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(buckle_core::error::user(
            format!("Downloading {url} returned HTTP {}.", response.status),
            &["Check the URL in the task's `deb` field."],
        ));
    }
    let path = state_dir(cx).join("downloads").join(file);
    cx.system
        .write_atomic(&path, &response.body, &FilePerms::mode(0o644))
        .await?;
    Ok(path)
}

buckle_core::register_task!(AptPackage);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::apt_common::testkit::Apt;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host(apt: &Apt) -> (FakeSystem, Cx) {
        let sys = apt.attach(FakeSystem::new());
        let cx = cx(&sys);
        (sys, cx)
    }

    #[rstest]
    #[case::present_and_installed(json!({"name": "nginx"}), Some("1.22.1-9"), None)]
    #[case::pinned_version_matches(json!({"name": "nginx", "version": "1.22.1-9"}), Some("1.22.1-9"), None)]
    #[case::wildcard_version_matches(json!({"name": "nginx", "version": "1.22.*"}), Some("1.22.1-9"), None)]
    #[case::absent_and_missing(json!({"name": "nginx", "state": "absent"}), None, None)]
    #[case::latest_and_current(json!({"name": "nginx", "state": "latest"}), Some("1.22.1-9"), Some("1.22.1-9"))]
    #[case::latest_with_no_candidate(json!({"name": "nginx", "state": "latest"}), Some("1.22.1-9"), None)]
    #[tokio::test]
    async fn plan_is_a_noop_when_the_host_already_matches(
        #[case] spec_json: serde_json::Value,
        #[case] installed: Option<&str>,
        #[case] candidate: Option<&str>,
    ) {
        let mut apt = Apt::new();
        if let Some(v) = installed {
            apt = apt.installed("nginx", v);
        }
        if let Some(c) = candidate {
            apt = apt.candidate("nginx", c);
        }
        let (_sys, cx) = host(&apt);
        let planned = AptPackage.plan(&cx, &spec(spec_json)).await.unwrap();
        assert_eq!(planned, Planned::NoOp);
    }

    #[rstest]
    #[case::missing(json!({"name": "nginx"}), None, None, "install nginx")]
    #[case::wrong_version(json!({"name": "nginx", "version": "1.24.0"}), Some("1.22.1-9"), None, "change nginx 1.22.1-9 -> 1.24.0")]
    #[case::installed_but_absent_wanted(json!({"name": "nginx", "state": "absent"}), Some("1.22.1-9"), None, "remove nginx 1.22.1-9")]
    #[case::newer_candidate(json!({"name": "nginx", "state": "latest"}), Some("1.22.1-9"), Some("1.22.1-9+deb12u1"), "change nginx 1.22.1-9 -> 1.22.1-9+deb12u1")]
    #[case::latest_missing(json!({"name": "nginx", "state": "latest"}), None, Some("1.22.1-9"), "install nginx")]
    #[tokio::test]
    async fn plan_is_a_change_when_the_host_differs(
        #[case] spec_json: serde_json::Value,
        #[case] installed: Option<&str>,
        #[case] candidate: Option<&str>,
        #[case] summary: &str,
    ) {
        let mut apt = Apt::new();
        if let Some(v) = installed {
            apt = apt.installed("nginx", v);
        }
        if let Some(c) = candidate {
            apt = apt.candidate("nginx", c);
        }
        let (_sys, cx) = host(&apt);
        let planned = AptPackage.plan(&cx, &spec(spec_json)).await.unwrap();
        match planned {
            Planned::Change { diff } => assert_eq!(diff.summary(), summary),
            other => panic!("expected a change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn config_files_only_counts_as_not_installed() {
        let apt = Apt::new();
        apt.0
            .lock()
            .unwrap()
            .config_files
            .insert("nginx".into(), "1.22.1-9".into());
        let (_sys, cx) = host(&apt);
        let present = AptPackage
            .plan(&cx, &spec(json!({"name": "nginx"})))
            .await
            .unwrap();
        assert!(present.is_change());
        let absent = AptPackage
            .plan(&cx, &spec(json!({"name": "nginx", "state": "absent"})))
            .await
            .unwrap();
        assert_eq!(absent, Planned::NoOp);
    }

    #[rstest]
    #[case(json!({"name": "linux-image", "version": "6.1.0-20"}))]
    #[case(json!({"name": "linux-image", "state": "latest"}))]
    #[case(json!({"name": "linux-image", "state": "absent"}))]
    #[tokio::test]
    async fn held_packages_block_changes(#[case] spec_json: serde_json::Value) {
        let apt = Apt::new()
            .held("linux-image", "6.1.0-18")
            .candidate("linux-image", "6.1.0-20");
        let (_sys, cx) = host(&apt);
        match AptPackage.plan(&cx, &spec(spec_json)).await.unwrap() {
            Planned::Blocked { reason } => assert!(reason.contains("held"), "{reason}"),
            other => panic!("expected blocked, got {other:?}"),
        }
    }

    #[rstest]
    #[case(json!({}))]
    #[case(json!({"names": ["a", "b"], "version": "1"}))]
    #[case(json!({"name": "a", "state": "latest", "version": "1"}))]
    #[case(json!({"names": ["a", "b"], "deb": "https://x/a.deb"}))]
    #[tokio::test]
    async fn invalid_specs_are_user_errors(#[case] spec_json: serde_json::Value) {
        let (_sys, cx) = host(&Apt::new());
        let err = AptPackage.plan(&cx, &spec(spec_json)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User), "{err:?}");
    }

    #[tokio::test]
    async fn apply_installs_then_the_next_plan_is_a_noop() {
        let apt = Apt::new()
            .candidate("nginx", "1.22.1-9")
            .candidate("curl", "7.88.1-10");
        let (sys, cx) = host(&apt);
        let s: Spec = spec(json!({"names": ["nginx", "curl"], "install_recommends": false}));
        assert!(AptPackage.plan(&cx, &s).await.unwrap().is_change());
        let applied = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(applied.changed);
        assert_eq!(AptPackage.plan(&cx, &s).await.unwrap(), Planned::NoOp);

        let install = sys
            .commands_run()
            .into_iter()
            .find(|c| c.program == "apt-get")
            .unwrap();
        assert_eq!(install.env["DEBIAN_FRONTEND"], "noninteractive");
        assert_eq!(&install.args[..2], &["install", "-y"]);
        assert!(install.args.contains(&"--no-install-recommends".to_owned()));
        assert_eq!(&install.args[install.args.len() - 2..], &["nginx", "curl"]);
        assert_eq!(applied.artefacts.len(), 2);
    }

    #[tokio::test]
    async fn apply_pins_the_requested_version_and_can_refresh_the_cache_first() {
        let apt = Apt::new();
        let (sys, cx) = host(&apt);
        let s: Spec = spec(json!({"name": "nomad", "version": "1.7.5-1", "update_cache": true}));
        AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let lines = sys.command_lines();
        let apt_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with("apt-get")).collect();
        assert!(apt_lines[0].starts_with("apt-get update"), "{apt_lines:?}");
        assert!(apt_lines[1].ends_with("nomad=1.7.5-1"), "{apt_lines:?}");
        assert_eq!(apt.version_of("nomad").as_deref(), Some("1.7.5-1"));
        assert_eq!(apt.updates(), 1);
        assert_eq!(AptPackage.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn state_absent_removes_installed_packages() {
        let apt = Apt::new().installed("telnet", "0.17-44");
        let (sys, cx) = host(&apt);
        let s: Spec = spec(json!({"name": "telnet", "state": "absent"}));
        let applied = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(applied.changed);
        assert!(applied.artefacts.is_empty());
        assert!(
            sys.command_lines()
                .iter()
                .any(|l| l.starts_with("apt-get remove -y"))
        );
        assert_eq!(AptPackage.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn only_packages_buckle_installed_are_recorded_and_removed() {
        // curl was already on the host; nginx is new.
        let apt = Apt::new()
            .installed("curl", "7.88.1-10")
            .candidate("nginx", "1.22.1-9");
        let (sys, cx) = host(&apt);
        let s: Spec = spec(json!({"names": ["nginx", "curl"]}));
        let applied = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Package {
                manager: "apt".into(),
                name: "nginx".into()
            }]
        );
        let record = record_of("apt.package", "nginx,curl", &applied);
        assert_eq!(AptPackage.describe_removal(&record), "apt-get remove nginx");
        let removed = AptPackage.remove(&cx, &record).await.unwrap();
        assert!(removed.changed);
        assert!(!apt.is_installed("nginx"));
        assert!(apt.is_installed("curl"), "pre-existing packages stay");
        let remove_cmd = sys
            .commands_run()
            .into_iter()
            .rfind(|c| c.args.first().map(String::as_str) == Some("remove"))
            .unwrap();
        assert_eq!(remove_cmd.args.last().map(String::as_str), Some("nginx"));
    }

    #[tokio::test]
    async fn removal_with_nothing_installed_by_buckle_does_nothing() {
        let apt = Apt::new().installed("curl", "7.88.1-10");
        let (sys, cx) = host(&apt);
        let s: Spec = spec(json!({"name": "curl"}));
        let applied = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(applied.artefacts.is_empty());
        let record = record_of("apt.package", "curl", &applied);
        assert!(
            AptPackage
                .describe_removal(&record)
                .contains("leave curl installed")
        );
        let removed = AptPackage.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert!(apt.is_installed("curl"));
        assert!(
            !sys.command_lines()
                .iter()
                .any(|l| l.starts_with("apt-get remove"))
        );
    }

    #[tokio::test]
    async fn ownership_survives_a_later_version_bump() {
        let apt = Apt::new().candidate("nomad", "1.7.5-1");
        let (_sys, cx) = host(&apt);
        let v1: Spec = spec(json!({"name": "nomad"}));
        let first = AptPackage.apply(&cx, &v1, &Planned::NoOp).await.unwrap();
        assert_eq!(first.artefacts.len(), 1);
        // The author later pins a different version: the package is already
        // installed when the second apply runs, but Buckle still owns it.
        let v2: Spec = spec(json!({"name": "nomad", "version": "1.8.0-1"}));
        let second = AptPackage.apply(&cx, &v2, &Planned::NoOp).await.unwrap();
        assert_eq!(second.artefacts, first.artefacts);
        assert_eq!(apt.version_of("nomad").as_deref(), Some("1.8.0-1"));
    }

    #[tokio::test]
    async fn deb_urls_are_downloaded_installed_and_cleaned_up() {
        let apt = Apt::new();
        let sys = apt
            .attach(FakeSystem::new())
            .on_http("https://example.com/debs/", |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: b"!<arch>".to_vec(),
            });
        let cx = cx(&sys);
        let s: Spec = spec(
            json!({"name": "grafana-agent", "deb": "https://example.com/debs/grafana-agent_1.0_amd64.deb?x=1"}),
        );
        let applied = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let install = sys
            .command_lines()
            .into_iter()
            .find(|l| l.starts_with("apt-get install"))
            .unwrap();
        assert!(
            install
                .replace('\\', "/")
                .ends_with("/var/lib/buckle/downloads/grafana-agent_1.0_amd64.deb"),
            "{install}"
        );
        assert!(!sys.exists("/var/lib/buckle/downloads/grafana-agent_1.0_amd64.deb"));
        assert_eq!(applied.artefacts.len(), 1);
        assert_eq!(AptPackage.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn failed_apt_commands_surface_stderr() {
        let sys = FakeSystem::new()
            .on_command_fail(
                "dpkg-query",
                &["-W"],
                1,
                "dpkg-query: no packages found matching nope",
            )
            .on_command_fail(
                "apt-get",
                &["install"],
                100,
                "E: Unable to locate package nope",
            );
        let cx = cx(&sys);
        let s: Spec = spec(json!({"name": "nope"}));
        let err = AptPackage.apply(&cx, &s, &Planned::NoOp).await.unwrap_err();
        assert!(
            err.description().contains("Unable to locate package nope"),
            "{}",
            err.description()
        );
    }

    #[test]
    fn kind_metadata() {
        assert_eq!(AptPackage::KIND, "apt.package");
        assert_eq!(AptPackage::LOCKS, &["apt"]);
        assert!(
            buckle_core::task::lookup("apt.package")
                .unwrap()
                .removal_caution()
        );
        assert_eq!(AptPackage::key(&spec(json!({"names": ["a", "b"]}))), "a,b");
    }
}
