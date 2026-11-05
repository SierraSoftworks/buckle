//! `apt.upgrade`: upgrade every upgradable Debian package (usually an action).

use super::apt_common::apt_get;
use super::sys_exec::{first_lines, run_checked};
use async_trait::async_trait;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Upgrades the host's packages with `apt-get dist-upgrade` (or `upgrade`).
///
/// ```yaml
/// actions:
///   upgrade:
///     apt.upgrade: { autoremove: true }
///     notify: [reboot-if-required]
/// handlers:
///   reboot-if-required: { system.reboot: { only_if: "test -f /run/reboot-required" } }
/// ```
///
/// The plan lists the packages a simulation (`apt-get -s`) would upgrade,
/// install or remove, from the package index as it is: planning never
/// refreshes it. With `update_cache` (the default) the plan is always a
/// change, since the refreshed index may offer more than the plan can see,
/// and applying runs `apt-get update` first; without it, nothing to upgrade
/// plans as no change. Held packages are left alone, as apt leaves them.
///
/// Meant for `actions:` (on demand, or across the fleet as an action
/// rollout). As a task it would upgrade on every apply. Nothing is undone on
/// removal.
pub struct AptUpgrade;
buckle_core::register_task!(AptUpgrade);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Run `apt-get update` before upgrading (default true).
    #[serde(default = "default_true")]
    pub update_cache: bool,
    /// `dist-upgrade` (default: installs and removes what new dependencies
    /// need) rather than `upgrade` (never adds or removes a package).
    #[serde(default = "default_true")]
    pub dist: bool,
    /// Run `apt-get autoremove` afterwards.
    #[serde(default)]
    pub autoremove: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            update_cache: true,
            dist: true,
            autoremove: false,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Spec {
    fn subcommand(&self) -> &'static str {
        if self.dist { "dist-upgrade" } else { "upgrade" }
    }
}

/// What a simulated upgrade would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Simulation {
    /// `name old -> new`, or `name new` for a package new to the host.
    pub upgrades: Vec<String>,
    pub installs: Vec<String>,
    pub removals: Vec<String>,
}

impl Simulation {
    fn is_empty(&self) -> bool {
        self.upgrades.is_empty() && self.installs.is_empty() && self.removals.is_empty()
    }

    fn count(&self) -> usize {
        self.upgrades.len() + self.installs.len() + self.removals.len()
    }

    /// One line: what changes, the first few packages named.
    fn summary(&self) -> String {
        const SHOWN: usize = 8;
        let mut parts = Vec::new();
        for (verb, list) in [
            ("upgrade", &self.upgrades),
            ("install", &self.installs),
            ("remove", &self.removals),
        ] {
            if list.is_empty() {
                continue;
            }
            let mut text = list
                .iter()
                .take(SHOWN)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if list.len() > SHOWN {
                text.push_str(&format!(" and {} more", list.len() - SHOWN));
            }
            parts.push(format!("{verb} {} ({text})", plural(list.len())));
        }
        parts.join("; ")
    }
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 package".into()
    } else {
        format!("{n} packages")
    }
}

/// Parses the `Inst` and `Remv` lines of an `apt-get -s` run:
/// `Inst libssl3 [3.0.11-1] (3.0.11-2 Debian:12.5/stable [amd64])`.
pub(crate) fn parse_simulation(text: &str) -> Simulation {
    let mut sim = Simulation::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Inst ") {
            let mut words = rest.split_whitespace();
            let Some(name) = words.next() else { continue };
            let name = name.split(':').next().unwrap_or(name);
            let rest: Vec<&str> = words.collect();
            let old = rest
                .first()
                .and_then(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')));
            let new = rest
                .iter()
                .find_map(|w| w.strip_prefix('('))
                .map(|w| w.trim_end_matches(')'));
            match (old, new) {
                (Some(old), Some(new)) => sim.upgrades.push(format!("{name} {old} -> {new}")),
                (Some(_), None) => sim.upgrades.push(name.to_owned()),
                (None, Some(new)) => sim.installs.push(format!("{name} {new}")),
                (None, None) => sim.installs.push(name.to_owned()),
            }
        } else if let Some(rest) = line.strip_prefix("Remv ")
            && let Some(name) = rest.split_whitespace().next()
        {
            sim.removals
                .push(name.split(':').next().unwrap_or(name).to_owned());
        }
    }
    sim
}

/// What `apt-get -s <subcommand>` says the upgrade would do. Pure: a
/// simulation changes nothing.
async fn simulate(cx: &Cx, spec: &Spec) -> Result<Simulation> {
    let out = cx
        .system
        .run(apt_get(spec.subcommand(), false).arg("-s"))
        .await?;
    if !out.success() {
        return Err(buckle_core::error::user(
            format!(
                "Simulating `apt-get {}` failed: {}",
                spec.subcommand(),
                first_lines(&out.stderr_str(), &out.stdout_str(), 4)
            ),
            &[
                "Run `apt-get -s dist-upgrade` on the host to see why apt cannot upgrade.",
                "A broken dpkg state needs `dpkg --configure -a` first.",
            ],
        ));
    }
    Ok(parse_simulation(&out.stdout_str()))
}

#[async_trait]
impl Task for AptUpgrade {
    type Spec = Spec;
    const KIND: &'static str = "apt.upgrade";
    const LOCKS: &'static [&'static str] = &["apt"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str =
        "Upgrade every upgradable Debian package (apt-get dist-upgrade); meant for actions.";

    fn key(_spec: &Spec) -> String {
        "upgrade".into()
    }

    fn describe(spec: &Spec) -> String {
        format!("apt-get {}", spec.subcommand())
    }

    #[tracing::instrument(name = "task.apt.upgrade.plan", skip_all)]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        if let Some(blocked) = super::apt_common::unavailable(cx, true).await? {
            return Ok(blocked);
        }
        let sim = simulate(cx, spec).await?;
        let known = if sim.is_empty() {
            "nothing upgradable in the current index".to_owned()
        } else {
            sim.summary()
        };
        if spec.update_cache {
            return Ok(Planned::change(Diff::Opaque {
                summary: format!("apt-get update, then {}: {known}", spec.subcommand()),
            }));
        }
        if sim.is_empty() {
            return Ok(Planned::NoOp);
        }
        Ok(Planned::change(Diff::Opaque { summary: known }))
    }

    #[tracing::instrument(name = "task.apt.upgrade.apply", skip_all)]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        if spec.update_cache {
            run_checked(cx, apt_get("update", false)).await?;
        }
        let sim = simulate(cx, spec).await?;
        if sim.is_empty() {
            return Ok(Applied::unchanged("every package is up to date"));
        }
        run_checked(cx, apt_get(spec.subcommand(), true)).await?;
        if spec.autoremove {
            run_checked(cx, apt_get("autoremove", true)).await?;
        }
        Ok(Applied::changed(format!(
            "{} changed: {}",
            plural(sim.count()),
            sim.summary()
        )))
    }

    fn describe_removal(&self, _record: &buckle_core::state::ResourceRecord) -> String {
        "nothing (upgraded packages stay upgraded)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::testing::{FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;

    const SIM: &str = "\
NOTE: This is only a simulation!
Reading package lists...
Inst libssl3 [3.0.11-1~deb12u1] (3.0.11-1~deb12u2 Debian:12.5/stable [amd64])
Inst linux-image-6.1.0-21-amd64 (6.1.90-1 Debian-Security:12/stable-security [amd64])
Remv linux-image-6.1.0-18-amd64 [6.1.76-1]
Conf libssl3 (3.0.11-1~deb12u2 Debian:12.5/stable [amd64])
";

    /// A host whose simulation prints `sim`.
    fn host(sim: &'static str) -> FakeSystem {
        FakeSystem::new()
            .with_program("dpkg-query")
            .on_command("apt-get", &[], move |cmd| {
                if cmd.args.iter().any(|a| a == "-s") {
                    ok_output(sim)
                } else {
                    ok_output("")
                }
            })
    }

    #[test]
    fn simulations_parse_into_upgrades_installs_and_removals() {
        let sim = parse_simulation(SIM);
        assert_eq!(
            sim.upgrades,
            vec!["libssl3 3.0.11-1~deb12u1 -> 3.0.11-1~deb12u2"]
        );
        assert_eq!(sim.installs, vec!["linux-image-6.1.0-21-amd64 6.1.90-1"]);
        assert_eq!(sim.removals, vec!["linux-image-6.1.0-18-amd64"]);
        assert!(sim.summary().contains("upgrade 1 package"));
    }

    #[rstest]
    #[case::refreshing_always_plans_a_change(json!({}), "", true)]
    #[case::without_refresh_nothing_is_no_change(json!({"update_cache": false}), "", false)]
    #[case::without_refresh_upgrades_are_a_change(json!({"update_cache": false}), SIM, true)]
    #[tokio::test]
    async fn plans_from_a_simulation(
        #[case] raw: serde_json::Value,
        #[case] sim: &'static str,
        #[case] change: bool,
    ) {
        let sys = host(sim);
        let planned = AptUpgrade.plan(&cx(&sys), &spec(raw)).await.unwrap();
        assert_eq!(planned.is_change(), change, "{planned:?}");
        assert!(
            sys.command_lines().iter().all(|l| l.contains(" -s")),
            "planning only simulates: {:?}",
            sys.command_lines()
        );
    }

    #[tokio::test]
    async fn applying_refreshes_upgrades_and_autoremoves() {
        let sys = host(SIM);
        let s: Spec = spec(json!({"autoremove": true}));
        let applied = AptUpgrade
            .apply(&cx(&sys), &s, &Planned::NoOp)
            .await
            .unwrap();
        assert!(applied.changed);
        let lines = sys.command_lines();
        let first_word = |l: &String| l.split_whitespace().nth(1).map(str::to_owned);
        let ran: Vec<String> = lines
            .iter()
            .filter(|l| !l.contains(" -s"))
            .filter_map(first_word)
            .collect();
        assert_eq!(ran, vec!["update", "dist-upgrade", "autoremove"]);
    }

    #[tokio::test]
    async fn nothing_to_upgrade_changes_nothing() {
        let sys = host("Reading package lists...\n");
        let applied = AptUpgrade
            .apply(&cx(&sys), &Spec::default(), &Planned::NoOp)
            .await
            .unwrap();
        assert!(!applied.changed);
        assert!(
            !sys.command_lines()
                .iter()
                .any(|l| l.contains("dist-upgrade -y"))
        );
    }

    #[tokio::test]
    async fn hosts_without_apt_are_blocked() {
        let sys = FakeSystem::new();
        let planned = AptUpgrade.plan(&cx(&sys), &Spec::default()).await.unwrap();
        assert!(matches!(planned, Planned::Blocked { .. }));
    }
}
