//! Parsers and command builders shared by the `apt.*` task kinds.

use buckle_core::Result;
use buckle_core::system::Command;
use buckle_core::task::Cx;
use std::time::Duration;

/// How long any single apt-get invocation may run.
pub(crate) const APT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// An `apt-get` invocation that never prompts and waits for the dpkg lock.
pub(crate) fn apt_get(subcommand: &str, assume_yes: bool) -> Command {
    let mut cmd = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("APT_LISTCHANGES_FRONTEND", "none")
        .arg(subcommand)
        .timeout(APT_TIMEOUT);
    if assume_yes {
        cmd = cmd.arg("-y");
    }
    cmd.args([
        "-o",
        "DPkg::Lock::Timeout=120",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
    ])
}

/// The packages `apt-get remove <pkgs>` would remove besides `pkgs`
/// themselves (packages that depend on them), from a simulation
/// (`apt-get -s remove`), which changes nothing.
pub(crate) async fn removal_takes_along(cx: &Cx, pkgs: &[String]) -> Result<Vec<String>> {
    let out = cx
        .system
        .run(
            apt_get("remove", false)
                .arg("-s")
                .args(pkgs.iter().cloned()),
        )
        .await?;
    if !out.success() {
        return Err(buckle_core::error::user(
            format!(
                "Simulating `apt-get remove {}` failed: {}",
                pkgs.join(" "),
                super::sys_exec::first_lines(&out.stderr_str(), &out.stdout_str(), 4)
            ),
            &["Run `apt-get -s remove` by hand to see why apt cannot remove the package."],
        ));
    }
    Ok(parse_simulated_removals(&out.stdout_str())
        .into_iter()
        .filter(|p| !pkgs.contains(p))
        .collect())
}

/// Package names from the `Remv <name> [<version>]` lines of a simulation.
pub(crate) fn parse_simulated_removals(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("Remv "))
        .filter_map(|rest| rest.split_whitespace().next())
        // `name:arch` on multi-arch hosts.
        .map(|n| n.split(':').next().unwrap_or(n).to_owned())
        .collect();
    out.dedup();
    out
}

/// What `dpkg-query -W -f='${Status} ${Version}\n'` says about a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PkgState {
    /// Unpacked and configured (also when only triggers are pending).
    Installed { version: String, held: bool },
    /// Removed but its configuration files remain (`apt-get remove` without purge).
    ConfigFiles { version: String },
    /// Not on the system (including packages dpkg has never heard of).
    NotInstalled,
    /// Half-installed, half-configured or otherwise needing `dpkg --configure -a`.
    Broken { state: String, version: String },
}

impl PkgState {
    pub(crate) fn is_installed(&self) -> bool {
        matches!(self, PkgState::Installed { .. })
    }
}

/// Parses one status line such as `install ok installed 1.22.1-9+deb12u1`.
/// Returns `None` for lines that are not `<want> <flag> <state> [<version>]`.
pub(crate) fn parse_dpkg_status(line: &str) -> Option<PkgState> {
    let mut parts = line.split_whitespace();
    let want = parts.next()?;
    let flag = parts.next()?;
    let state = parts.next()?;
    if !matches!(want, "install" | "hold" | "deinstall" | "purge" | "unknown")
        || !matches!(flag, "ok" | "reinstreq" | "hold" | "hold-reinstreq")
    {
        return None;
    }
    let version = parts.next().unwrap_or("").to_owned();
    Some(match state {
        "installed" | "triggers-pending" | "triggers-awaited" => PkgState::Installed {
            version,
            held: want == "hold",
        },
        "config-files" => PkgState::ConfigFiles { version },
        "not-installed" => PkgState::NotInstalled,
        other => PkgState::Broken {
            state: other.to_owned(),
            version,
        },
    })
}

/// Asks dpkg about one package.
pub(crate) async fn query_package(cx: &Cx, name: &str) -> Result<PkgState> {
    let out = cx
        .system
        .run(
            Command::new("dpkg-query")
                .arg("-W")
                .arg("-f=${Status} ${Version}\\n")
                .arg(name),
        )
        .await?;
    let stdout = out.stdout_str();
    if let Some(state) = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(parse_dpkg_status)
    {
        return Ok(state);
    }
    if out.code == Some(1) {
        // "dpkg-query: no packages found matching <name>"
        return Ok(PkgState::NotInstalled);
    }
    Err(buckle_core::error::user(
        format!(
            "`dpkg-query -W {name}` failed: {}",
            super::sys_exec::first_lines(&out.stderr_str(), &stdout, 3)
        ),
        &["Check that dpkg is healthy (`dpkg --audit`)."],
    ))
}

/// The relevant parts of `apt-cache policy <pkg>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    pub installed: Option<String>,
    pub candidate: Option<String>,
}

/// Parses `apt-cache policy` output; `(none)` becomes `None`.
pub(crate) fn parse_policy(text: &str) -> Policy {
    let mut policy = Policy::default();
    for line in text.lines() {
        let line = line.trim();
        let field = |prefix: &str| {
            line.strip_prefix(prefix)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty() && v != "(none)")
        };
        if let Some(v) = field("Installed:") {
            policy.installed = Some(v);
        } else if let Some(v) = field("Candidate:") {
            policy.candidate = Some(v);
        }
    }
    policy
}

pub(crate) async fn query_policy(cx: &Cx, name: &str) -> Result<Policy> {
    let out = cx
        .system
        .run(Command::new("apt-cache").args(["policy", name]))
        .await?;
    Ok(parse_policy(&out.stdout_str()))
}

/// Matches a dpkg version against a requested one. `*` in the request is a wildcard
/// (`1.22.*`); anything else must match exactly.
pub(crate) fn version_matches(installed: &str, wanted: &str) -> bool {
    fn glob(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
            Some((c, rest)) => text.first() == Some(c) && glob(rest, &text[1..]),
        }
    }
    glob(wanted.as_bytes(), installed.as_bytes())
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A stateful pretend apt/dpkg for task tests: `dpkg-query`, `apt-cache policy`
    //! and `apt-get` respond according to (and mutate) an in-memory package set.

    use buckle_core::system::Output;
    use buckle_core::testing::{FakeSystem, ok_output};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct World {
        /// name → (version, held)
        pub installed: BTreeMap<String, (String, bool)>,
        pub config_files: BTreeMap<String, String>,
        /// name → candidate version
        pub candidates: BTreeMap<String, String>,
        pub updates: usize,
        pub deb_names: BTreeSet<String>,
        /// package → packages that depend on it (and go with it on removal)
        pub dependents: BTreeMap<String, Vec<String>>,
    }

    #[derive(Clone, Default)]
    pub struct Apt(pub Arc<Mutex<World>>);

    impl Apt {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn installed(self, name: &str, version: &str) -> Self {
            self.0
                .lock()
                .unwrap()
                .installed
                .insert(name.into(), (version.into(), false));
            self
        }

        pub fn held(self, name: &str, version: &str) -> Self {
            self.0
                .lock()
                .unwrap()
                .installed
                .insert(name.into(), (version.into(), true));
            self
        }

        pub fn candidate(self, name: &str, version: &str) -> Self {
            self.0
                .lock()
                .unwrap()
                .candidates
                .insert(name.into(), version.into());
            self
        }

        /// `dependent` depends on `on`, so removing `on` removes `dependent` too.
        pub fn depends(self, dependent: &str, on: &str) -> Self {
            self.0
                .lock()
                .unwrap()
                .dependents
                .entry(on.into())
                .or_default()
                .push(dependent.into());
            self
        }

        pub fn version_of(&self, name: &str) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .installed
                .get(name)
                .map(|(v, _)| v.clone())
        }

        pub fn is_installed(&self, name: &str) -> bool {
            self.version_of(name).is_some()
        }

        pub fn updates(&self) -> usize {
            self.0.lock().unwrap().updates
        }

        /// Registers the three commands on `sys`.
        pub fn attach(&self, sys: FakeSystem) -> FakeSystem {
            let w = self.0.clone();
            let sys = sys.on_command("dpkg-query", &["-W"], move |cmd| {
                let name = cmd.args.last().cloned().unwrap_or_default();
                let w = w.lock().unwrap();
                if let Some((v, held)) = w.installed.get(&name) {
                    let want = if *held { "hold" } else { "install" };
                    ok_output(&format!("{want} ok installed {v}\n"))
                } else if let Some(v) = w.config_files.get(&name) {
                    ok_output(&format!("deinstall ok config-files {v}\n"))
                } else {
                    Output {
                        code: Some(1),
                        stdout: vec![],
                        stderr: format!("dpkg-query: no packages found matching {name}\n")
                            .into_bytes(),
                        timed_out: false,
                    }
                }
            });
            let w = self.0.clone();
            let sys = sys.on_command("apt-cache", &["policy"], move |cmd| {
                let name = cmd.args.last().cloned().unwrap_or_default();
                let w = w.lock().unwrap();
                let installed = w.installed.get(&name).map(|(v, _)| v.as_str());
                let candidate = w.candidates.get(&name).map(String::as_str).or(installed);
                match (installed, candidate) {
                    (None, None) => ok_output(""),
                    (i, c) => ok_output(&format!(
                        "{name}:\n  Installed: {}\n  Candidate: {}\n  Version table:\n",
                        i.unwrap_or("(none)"),
                        c.unwrap_or("(none)")
                    )),
                }
            });
            let w = self.0.clone();
            sys.on_command("apt-get", &[], move |cmd| {
                let mut w = w.lock().unwrap();
                let sub = cmd.args.first().cloned().unwrap_or_default();
                let mut targets: Vec<String> = Vec::new();
                let simulate = cmd.args.iter().any(|a| a == "-s");
                let mut args = cmd.args.iter().skip(1);
                while let Some(a) = args.next() {
                    if a == "-o" {
                        args.next();
                    } else if !a.starts_with('-') {
                        targets.push(a.clone());
                    }
                }
                match sub.as_str() {
                    "update" => w.updates += 1,
                    "install" => {
                        for t in &targets {
                            let (name, version) = match t.split_once('=') {
                                Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
                                None if t.starts_with("./") || t.starts_with('/') => {
                                    let stem = t
                                        .rsplit('/')
                                        .next()
                                        .unwrap_or(t)
                                        .trim_end_matches(".deb")
                                        .split('_')
                                        .next()
                                        .unwrap_or("")
                                        .to_owned();
                                    w.deb_names.insert(stem.clone());
                                    (stem, Some("1.0.0".to_owned()))
                                }
                                None => (t.clone(), None),
                            };
                            let version = version
                                .or_else(|| w.candidates.get(&name).cloned())
                                .unwrap_or_else(|| "1.0".into());
                            w.config_files.remove(&name);
                            w.installed.insert(name, (version, false));
                        }
                    }
                    "remove" => {
                        let mut all = targets.clone();
                        for t in &targets {
                            for d in w.dependents.get(t).cloned().unwrap_or_default() {
                                if w.installed.contains_key(&d) && !all.contains(&d) {
                                    all.push(d);
                                }
                            }
                        }
                        if simulate {
                            let text: String = all
                                .iter()
                                .filter_map(|t| {
                                    w.installed.get(t).map(|(v, _)| format!("Remv {t} [{v}]\n"))
                                })
                                .collect();
                            return ok_output(&format!("NOTE: This is only a simulation!\n{text}"));
                        }
                        for t in &all {
                            if let Some((v, _)) = w.installed.remove(t) {
                                w.config_files.insert(t.clone(), v);
                            }
                        }
                    }
                    _ => {}
                }
                ok_output("")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(
        "install ok installed 1.22.1-9+deb12u1",
        PkgState::Installed { version: "1.22.1-9+deb12u1".into(), held: false }
    )]
    #[case(
        "hold ok installed 6.1.0-18",
        PkgState::Installed { version: "6.1.0-18".into(), held: true }
    )]
    #[case(
        "deinstall ok config-files 1.22.1-9",
        PkgState::ConfigFiles { version: "1.22.1-9".into() }
    )]
    #[case("unknown ok not-installed ", PkgState::NotInstalled)]
    #[case("install ok triggers-pending 2.36-9", PkgState::Installed { version: "2.36-9".into(), held: false })]
    #[case(
        "install reinstreq half-configured 2.0",
        PkgState::Broken { state: "half-configured".into(), version: "2.0".into() }
    )]
    fn dpkg_status_lines_parse(#[case] line: &str, #[case] expected: PkgState) {
        assert_eq!(parse_dpkg_status(line), Some(expected));
    }

    #[test]
    fn dpkg_status_rejects_garbage() {
        assert_eq!(parse_dpkg_status(""), None);
        assert_eq!(parse_dpkg_status("dpkg-query: no packages found"), None);
    }

    const POLICY_UPGRADABLE: &str = "\
nginx:
  Installed: 1.22.1-9
  Candidate: 1.22.1-9+deb12u1
  Version table:
     1.22.1-9+deb12u1 500
        500 http://deb.debian.org/debian bookworm/main amd64 Packages
        500 http://deb.debian.org/debian-security bookworm-security/main amd64 Packages
 *** 1.22.1-9 100
        100 /var/lib/dpkg/status
";

    const POLICY_UNINSTALLED: &str = "\
nomad:
  Installed: (none)
  Candidate: 1.7.5-1
  Version table:
     1.7.5-1 500
        500 https://apt.releases.hashicorp.com bookworm/main amd64 Packages
";

    #[rstest]
    #[case(POLICY_UPGRADABLE, Some("1.22.1-9"), Some("1.22.1-9+deb12u1"))]
    #[case(POLICY_UNINSTALLED, None, Some("1.7.5-1"))]
    #[case("", None, None)]
    #[case("ghost:\n  Installed: (none)\n  Candidate: (none)\n", None, None)]
    fn apt_cache_policy_parses(
        #[case] text: &str,
        #[case] installed: Option<&str>,
        #[case] candidate: Option<&str>,
    ) {
        let p = parse_policy(text);
        assert_eq!(p.installed.as_deref(), installed);
        assert_eq!(p.candidate.as_deref(), candidate);
    }

    #[rstest]
    #[case("1.22.1-9", "1.22.1-9", true)]
    #[case("1.22.1-9", "1.22.1-8", false)]
    #[case("1.22.1-9", "1.22.*", true)]
    #[case("1.22.1-9", "1.23.*", false)]
    #[case("1.22.1-9", "*-9", true)]
    #[case("1.22.1-9", "*-8", false)]
    #[case("1.22.1-9", "1.*.1-*", true)]
    fn versions_match_with_wildcards(
        #[case] installed: &str,
        #[case] wanted: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(version_matches(installed, wanted), expected);
    }

    #[test]
    fn apt_get_is_noninteractive() {
        let cmd = apt_get("install", true);
        assert_eq!(
            cmd.env.get("DEBIAN_FRONTEND").map(String::as_str),
            Some("noninteractive")
        );
        assert_eq!(&cmd.args[..2], &["install", "-y"]);
        assert!(!apt_get("update", false).args.contains(&"-y".to_owned()));
    }
}
