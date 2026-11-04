//! `github.release`: install a binary from a GitHub release.

use super::archive_extract::{self, EntryKind};
use super::hostkit::{self, describe_perms};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FileKind, FilePerms, HttpRequest};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Digest, Platform, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::instrument;

/// Installs a binary from a GitHub release, upgrading it when a newer one appears.
///
/// ```yaml
/// - github.release:
///     repo: atuinsh/atuin
///     version: latest                       # or "v18.3.0" / "18.3.0"
///     asset: "atuin-{arch}-unknown-linux-musl.tar.gz"  # {version} {arch} {os} {tag}
///     arch_map: { x86_64: x86_64, aarch64: aarch64 }   # host arch -> asset arch; armv7l is skipped
///     extract: atuin                        # file inside an archive asset
///     dest: /usr/local/bin/atuin
///     mode: "0755"
///     probe: ["atuin", "--version"]         # prints the installed version
///     probe_regex: 'atuin (\S+)'            # optional; default is the first x.y.z found
///     checksum_asset: "atuin-{arch}-unknown-linux-musl.tar.gz.sha256"
///     token_env: GITHUB_TOKEN               # optional; name of an environment variable
/// ```
///
/// The plan compares the version printed by `probe` with the wanted release
/// (`1.2.3 → 1.2.4`). `arch_map` extends the defaults `x86_64: amd64` and
/// `aarch64: arm64`; a host architecture with no mapping is reported as
/// blocked rather than failing, so a fleet can include hosts a release does not
/// support. `{os}` is `linux`, `darwin` or `windows`; `{version}` has no
/// leading `v`.
///
/// `version: latest` asks the GitHub API for the newest release on every plan,
/// which is rate limited to 60 requests an hour without a token; set
/// `token_env` to the name of an environment variable holding a token (the
/// value is only ever sent as an `Authorization` header). A pinned version
/// whose `probe` already reports it needs no API call. Without `probe`, an
/// existing `dest` counts as installed and is never upgraded (unless
/// `checksum_asset` is given for a plain binary asset).
///
/// Archive assets (`.tar.gz`, `.tgz`, `.tar`, `.zip`) need `extract`: the
/// path or file name of the binary inside. With `checksum_asset` (the name of a
/// `.sha256` or `SHA256SUMS` asset) the downloaded asset is verified before
/// anything is written. With `probe`, an install that does not then report the
/// release's version fails and puts the previous binary back, so an asset that
/// cannot run on the host (e.g. a glibc build on an older glibc) is reported
/// rather than reinstalled on every rollout. Removal deletes `dest` only if it
/// still matches what Buckle installed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// `owner/name` of the repository.
    pub repo: String,
    /// `latest`, `vX.Y.Z` or `X.Y.Z`.
    #[serde(default = "default_version")]
    pub version: String,
    /// Asset file name; may use `{version}`, `{tag}`, `{arch}` and `{os}`.
    pub asset: String,
    /// Host architecture (as `uname -m` prints it) to the asset's spelling; extends the defaults.
    #[serde(default)]
    pub arch_map: BTreeMap<String, String>,
    /// Absolute path to install the binary at.
    pub dest: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Command (argv) that prints the installed version.
    #[serde(default)]
    pub probe: Option<Vec<String>>,
    /// Regular expression selecting the version from the probe's output (first capture group, else the match).
    #[serde(default)]
    pub probe_regex: Option<String>,
    /// Name of a SHA-256 checksum asset in the release; may use the same placeholders as `asset`.
    #[serde(default)]
    pub checksum_asset: Option<String>,
    /// Name of an environment variable holding a GitHub token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// For archive assets: the binary's path or file name inside the archive.
    #[serde(default)]
    pub extract: Option<String>,
}

fn default_version() -> String {
    "latest".to_owned()
}

pub struct GithubRelease;
buckle_core::register_task!(GithubRelease);

fn default_arch(host: &str) -> Option<&'static str> {
    match host {
        "x86_64" | "amd64" => Some("amd64"),
        "aarch64" | "arm64" => Some("arm64"),
        _ => None,
    }
}

fn os_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "linux",
        Platform::MacOs => "darwin",
        Platform::Windows => "windows",
    }
}

fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".tar.gz", ".tgz", ".tar", ".zip"]
        .iter()
        .any(|e| lower.ends_with(e))
}

fn bare(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

/// A resolved release: which version, and where its asset lives.
#[derive(Debug, Clone)]
struct Resolved {
    version: String,
    asset_name: String,
    asset_url: String,
    checksum: Option<(String, String)>,
}

fn fill(template: &str, version: &str, tag: &str, arch: &str, os: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{tag}", tag)
        .replace("{arch}", arch)
        .replace("{os}", os)
}

impl GithubRelease {
    fn check_repo(spec: &Spec) -> Result<()> {
        let valid = spec.repo.split_once('/').is_some_and(|(o, n)| {
            !o.is_empty()
                && !n.is_empty()
                && !n.contains('/')
                && !spec.repo.contains(char::is_whitespace)
        });
        if valid {
            Ok(())
        } else {
            Err(error::user(
                format!("The repo '{}' is not in `owner/name` form.", spec.repo),
                &["Use the repository as it appears in its GitHub URL, e.g. `atuinsh/atuin`."],
            ))
        }
    }

    /// The asset-side architecture for this host, or `None` when the release has no build for it.
    async fn asset_arch(&self, cx: &Cx, spec: &Spec) -> (String, Option<String>) {
        let host = hostkit::host_arch(cx).await;
        let mapped = spec
            .arch_map
            .get(&host)
            .cloned()
            .or_else(|| default_arch(&host).map(str::to_owned));
        (host, mapped)
    }

    fn api_headers(&self, cx: &Cx, spec: &Spec, accept: &str) -> Result<Vec<(String, String)>> {
        let mut headers = vec![
            ("Accept".to_owned(), accept.to_owned()),
            ("User-Agent".to_owned(), "buckle".to_owned()),
            ("X-GitHub-Api-Version".to_owned(), "2022-11-28".to_owned()),
        ];
        if let Some(var) = &spec.token_env {
            let token = cx.system.env_var(var).filter(|t| !t.is_empty()).ok_or_else(|| {
                error::user(
                    format!("`token_env` names the environment variable {var}, which is not set for Buckle."),
                    &["Set the variable in the agent's environment, or remove `token_env` to use anonymous access."],
                )
            })?;
            headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
        }
        Ok(headers)
    }

    async fn release_json(&self, cx: &Cx, spec: &Spec) -> Result<Value> {
        let headers = self.api_headers(cx, spec, "application/vnd.github+json")?;
        let base = format!("https://api.github.com/repos/{}/releases", spec.repo);
        let parse = |body: Vec<u8>| -> Result<Value> {
            serde_json::from_slice(&body).map_err(|e| {
                error::wrap_user(
                    e,
                    "GitHub's release response could not be understood.",
                    &["Try again; if it persists, check that the repository has published releases."],
                )
            })
        };
        if spec.version.trim() == "latest" {
            return parse(hostkit::http_get(cx, &format!("{base}/latest"), &headers).await?);
        }
        let given = spec.version.trim();
        let candidates = if given.starts_with('v') {
            vec![given.to_owned(), given.trim_start_matches('v').to_owned()]
        } else {
            vec![format!("v{given}"), given.to_owned()]
        };
        let mut last = None;
        for tag in candidates {
            let request = HttpRequest {
                method: "GET".into(),
                url: format!("{base}/tags/{tag}"),
                headers: headers.clone(),
                ..Default::default()
            };
            let response = cx.system.http(request).await?;
            if (200..300).contains(&response.status) {
                return parse(response.body);
            }
            last = Some((tag, response.status));
        }
        let (tag, status) = last.unwrap_or_default();
        Err(error::user(
            format!(
                "GitHub has no release of {} tagged '{tag}' (HTTP {status}).",
                spec.repo
            ),
            &["Check the version against the repository's releases page."],
        ))
    }

    async fn resolve(&self, cx: &Cx, spec: &Spec, arch: &str) -> Result<Resolved> {
        let release = self.release_json(cx, spec).await?;
        let tag = release["tag_name"].as_str().ok_or_else(|| {
            error::user(
                format!("The latest release of {} has no tag name.", spec.repo),
                &["Check that the repository publishes releases (not only tags)."],
            )
        })?;
        let version = bare(tag).to_owned();
        let os = os_name(cx.platform());
        let asset_name = fill(&spec.asset, &version, tag, arch, os);
        let assets = release["assets"].as_array().cloned().unwrap_or_default();
        let find = |name: &str| -> Option<String> {
            let asset = assets.iter().find(|a| a["name"].as_str() == Some(name))?;
            // Authenticated downloads go through the API so private repositories work.
            let key = if spec.token_env.is_some() {
                "url"
            } else {
                "browser_download_url"
            };
            asset[key].as_str().map(str::to_owned)
        };
        let asset_url = find(&asset_name).ok_or_else(|| {
            let available: Vec<&str> = assets.iter().filter_map(|a| a["name"].as_str()).collect();
            error::user(
                format!(
                    "Release {tag} of {} has no asset named '{asset_name}'. Available: {}.",
                    spec.repo,
                    if available.is_empty() {
                        "none".to_owned()
                    } else {
                        available.join(", ")
                    }
                ),
                &["Adjust `asset` (and `arch_map`) so that it names an asset of the release."],
            )
        })?;
        let checksum = match &spec.checksum_asset {
            Some(template) => {
                let name = fill(template, &version, tag, arch, os);
                let url = find(&name).ok_or_else(|| {
                    error::user(
                        format!("Release {tag} of {} has no checksum asset named '{name}'.", spec.repo),
                        &["Adjust `checksum_asset` to name the release's .sha256 or SHA256SUMS file."],
                    )
                })?;
                Some((name, url))
            }
            None => None,
        };
        Ok(Resolved {
            version,
            asset_name,
            asset_url,
            checksum,
        })
    }

    /// What `probe` reports, or `None` when the task has no probe.
    async fn probe(&self, cx: &Cx, spec: &Spec) -> Result<Option<Probed>> {
        let Some(argv) = &spec.probe else {
            return Ok(None);
        };
        let Some(command) = Command::from_argv(argv) else {
            return Err(error::user(
                "The `probe` command is empty.",
                &["Give the program and its arguments, e.g. [\"atuin\", \"--version\"]."],
            ));
        };
        let line = command.display_line();
        // A probe that cannot run is how "not installed" looks; when `dest`
        // exists it is how a binary that does not suit the host looks.
        let out = match cx.system.run(command).await {
            Ok(out) => out,
            Err(_) => return Ok(Some(Probed::Failed(format!("`{line}` could not be run")))),
        };
        if !out.success() {
            let status = match (out.timed_out, out.code) {
                (true, _) => "timed out".to_owned(),
                (false, Some(code)) => format!("exited {code}"),
                (false, None) => "was killed by a signal".to_owned(),
            };
            let detail = out
                .stderr_str()
                .lines()
                .chain(out.stdout_str().lines())
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| format!(": {}", hostkit::clip(l, 200)))
                .unwrap_or_default();
            return Ok(Some(Probed::Failed(format!("`{line}` {status}{detail}"))));
        }
        let text = format!("{}\n{}", out.stdout_str(), out.stderr_str());
        let version = match &spec.probe_regex {
            Some(pattern) => {
                let re = hostkit::compile_regex(pattern, "probe_regex")?;
                re.captures(&text)
                    .and_then(|c| c.get(1).or_else(|| c.get(0)))
                    .map(|m| bare(m.as_str()).to_owned())
            }
            None => hostkit::find_version(&text),
        };
        Ok(Some(match version {
            Some(v) => Probed::Version(v),
            None => Probed::Failed(format!("`{line}` printed no version")),
        }))
    }

    /// Checks that the binary just written reports the release's version, and
    /// puts the previous binary back when it does not: a binary that cannot
    /// run on this host would otherwise be reinstalled on every rollout.
    async fn verify(
        &self,
        cx: &Cx,
        spec: &Spec,
        resolved: &Resolved,
        previous: Option<Vec<u8>>,
        perms: &FilePerms,
    ) -> Result<()> {
        let problem = match self.probe(cx, spec).await? {
            None => return Ok(()),
            Some(Probed::Version(v)) if v == resolved.version => return Ok(()),
            Some(Probed::Version(v)) => format!("reports version {v}"),
            Some(Probed::Failed(reason)) => reason,
        };
        let dest = spec.dest.display();
        let restored = match previous {
            Some(bytes) => {
                cx.system.write_atomic(&spec.dest, &bytes, perms).await?;
                "the previous binary was put back"
            }
            None => {
                cx.system.remove(&spec.dest, false).await?;
                "it was removed again"
            }
        };
        Err(error::user(
            format!(
                "{} {} was written to {dest}, but {problem}; {restored}.",
                spec.repo, resolved.version
            ),
            &[
                "Check that the asset suits this host: its architecture and C library (a glibc build needs at least the glibc it was built against; a musl build has no such need).",
                "Check that `probe` runs the binary at `dest` and that `probe_regex` selects the release's version.",
            ],
        ))
    }

    async fn download_headers(&self, cx: &Cx, spec: &Spec) -> Result<Vec<(String, String)>> {
        if spec.token_env.is_some() {
            self.api_headers(cx, spec, "application/octet-stream")
        } else {
            Ok(vec![("User-Agent".to_owned(), "buckle".to_owned())])
        }
    }
}

/// The outcome of running `probe`.
#[derive(Debug, Clone, PartialEq)]
enum Probed {
    Version(String),
    /// Why no version could be read.
    Failed(String),
}

impl Probed {
    fn version(&self) -> Option<&str> {
        match self {
            Probed::Version(v) => Some(v),
            Probed::Failed(_) => None,
        }
    }
}

struct Inspection {
    planned: Planned,
    resolved: Option<Resolved>,
}

impl GithubRelease {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        Self::check_repo(spec)?;
        let dest = spec.dest.display();
        let (host_arch, asset_arch) = self.asset_arch(cx, spec).await;
        let Some(arch) = asset_arch else {
            return Ok(Inspection {
                planned: Planned::blocked(format!(
                    "{} publishes no build for this host's architecture ({host_arch}); add it to `arch_map` if an asset exists",
                    spec.repo
                )),
                resolved: None,
            });
        };
        let meta = cx.system.stat(&spec.dest).await?;
        if let Some(m) = &meta
            && m.kind == FileKind::Directory
        {
            return Err(error::user(
                format!("{dest} is a directory; `dest` must be the binary's path."),
                &["Use a path such as /usr/local/bin/tool."],
            ));
        }
        let perm_changes = meta
            .as_ref()
            .map(|m| hostkit::perm_changes(m, &spec.perms))
            .unwrap_or_default();
        let settled = |perm_changes: Vec<String>| {
            if perm_changes.is_empty() {
                Planned::NoOp
            } else {
                Planned::change(Diff::Update {
                    summary: format!("{dest}: {}", perm_changes.join(", ")),
                    unified: None,
                })
            }
        };

        let probed = self.probe(cx, spec).await?;
        let installed = probed.as_ref().and_then(Probed::version).map(str::to_owned);
        let pinned = spec.version.trim() != "latest";
        if pinned && installed.as_deref() == Some(bare(&spec.version)) {
            return Ok(Inspection {
                planned: settled(perm_changes),
                resolved: None,
            });
        }
        if spec.probe.is_none() && meta.is_some() && spec.checksum_asset.is_none() {
            // Nothing to compare versions with: an existing binary counts as installed.
            return Ok(Inspection {
                planned: settled(perm_changes),
                resolved: None,
            });
        }

        let resolved = self.resolve(cx, spec, &arch).await?;
        let planned = match (&installed, &meta) {
            (Some(have), _) if *have == resolved.version => settled(perm_changes),
            (Some(have), _) => Planned::change(Diff::Update {
                summary: format!("{}: {have} → {}", spec.repo, resolved.version),
                unified: None,
            }),
            (None, Some(_)) if spec.probe.is_none() => {
                // No probe, but a checksum: the binary is current when it matches the release.
                match (&resolved.checksum, is_archive(&resolved.asset_name)) {
                    (Some((name, url)), false) => {
                        let headers = self.download_headers(cx, spec).await?;
                        let text = hostkit::http_get(cx, url, &headers).await?;
                        let want = hostkit::parse_sha256_text(
                            &String::from_utf8_lossy(&text),
                            &[resolved.asset_name.as_str(), name.as_str()],
                        );
                        let have = cx.system.read(&spec.dest).await?.map(|b| Digest::of(&b));
                        if want.is_some() && have.as_ref().map(Digest::as_str) == want.as_deref() {
                            settled(perm_changes)
                        } else {
                            Planned::change(Diff::Update {
                                summary: format!("{}: update to {}", spec.repo, resolved.version),
                                unified: None,
                            })
                        }
                    }
                    _ => settled(perm_changes),
                }
            }
            (None, Some(_)) => {
                // `dest` exists but does not report a version: reinstalling is
                // the fix, and the probe's failure says why it keeps recurring.
                let why = match &probed {
                    Some(Probed::Failed(reason)) => format!(" ({reason})"),
                    _ => String::new(),
                };
                Planned::change(Diff::Update {
                    summary: format!(
                        "reinstall {} {} at {dest}{why}",
                        spec.repo, resolved.version
                    ),
                    unified: None,
                })
            }
            (None, None) => Planned::change(Diff::Create {
                summary: format!("install {} {} to {dest}", spec.repo, resolved.version),
                preview: None,
            }),
        };
        Ok(Inspection {
            planned,
            resolved: Some(resolved),
        })
    }
}

#[async_trait]
impl Task for GithubRelease {
    type Spec = Spec;
    const KIND: &'static str = "github.release";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Install a binary from a GitHub release, with arch mapping, version probe and checksum verification.";

    fn key(spec: &Spec) -> String {
        spec.dest.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        let what = format!("{} {} at {}", spec.repo, spec.version, spec.dest.display());
        if perms.is_empty() {
            what
        } else {
            format!("{what} ({perms})")
        }
    }

    #[instrument(name = "task.github.release.plan", skip_all, fields(repo = %spec.repo, version = %spec.version, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.github.release.apply", skip_all, fields(repo = %spec.repo, version = %spec.version, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let inspection = self.inspect(cx, spec).await?;
        match &inspection.planned {
            Planned::NoOp => {
                let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
                return Ok(hostkit::ownership::file_written(
                    cx,
                    format!("{} already up to date", spec.dest.display()),
                    &spec.dest,
                    false,
                    Digest::of(&current),
                    None,
                    false,
                ));
            }
            Planned::Blocked { reason } => {
                return Err(error::user(
                    format!("Cannot install {}: {reason}.", spec.repo),
                    &[
                        "Add the host's architecture to `arch_map`, or select hosts the release supports.",
                    ],
                ));
            }
            Planned::Change { .. } => {}
        }
        let Some(resolved) = inspection.resolved else {
            // Only permissions differ.
            cx.system.set_perms(&spec.dest, &spec.perms).await?;
            let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
            // A binary Buckle did not install is adopted, not claimed.
            return Ok(hostkit::ownership::file_written(
                cx,
                format!("updated permissions of {}", spec.dest.display()),
                &spec.dest,
                false,
                Digest::of(&current),
                None,
                true,
            ));
        };

        let headers = self.download_headers(cx, spec).await?;
        let asset = hostkit::http_get(cx, &resolved.asset_url, &headers).await?;
        if let Some((name, url)) = &resolved.checksum {
            let text = hostkit::http_get(cx, url, &headers).await?;
            let want = hostkit::parse_sha256_text(
                &String::from_utf8_lossy(&text),
                &[resolved.asset_name.as_str()],
            )
            .ok_or_else(|| {
                error::user(
                    format!("No SHA-256 digest for '{}' was found in '{name}'.", resolved.asset_name),
                    &["The checksum asset should contain `<digest>  <filename>` lines or a single digest."],
                )
            })?;
            let have = Digest::of(&asset);
            if have.as_str() != want {
                return Err(error::user(
                    format!(
                        "The download of {} does not match its checksum: expected {want}, got {have}. Nothing was written to {}.",
                        resolved.asset_name,
                        spec.dest.display()
                    ),
                    &[
                        "Re-run later in case the release was being updated; if it persists, do not install it.",
                    ],
                ));
            }
        }

        let (binary, archive_mode) = if is_archive(&resolved.asset_name) {
            let wanted = spec.extract.as_deref().ok_or_else(|| {
                error::user(
                    format!(
                        "The asset '{}' is an archive, but `extract` is not set.",
                        resolved.asset_name
                    ),
                    &["Set `extract` to the binary's path or file name inside the archive."],
                )
            })?;
            let entries = archive_extract::read_archive(&asset, &resolved.asset_name)?;
            let wanted = wanted.trim_start_matches("./");
            let mut matches = entries.into_iter().filter(|e| {
                e.kind == EntryKind::File
                    && (e.original.to_string_lossy() == wanted
                        || e.original
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy() == wanted)
                        || e.original
                            .to_string_lossy()
                            .ends_with(&format!("/{wanted}")))
            });
            let found = matches.next().ok_or_else(|| {
                error::user(
                    format!("`{wanted}` is not in {}.", resolved.asset_name),
                    &["List the archive's contents and set `extract` to the binary's path."],
                )
            })?;
            if matches.next().is_some() {
                return Err(error::user(
                    format!(
                        "`{wanted}` matches more than one file in {}.",
                        resolved.asset_name
                    ),
                    &["Give the full path inside the archive in `extract`."],
                ));
            }
            (found.data, Some(found.mode))
        } else {
            (asset, None)
        };

        let existing = cx.system.stat(&spec.dest).await?;
        let previous = match &existing {
            Some(_) => cx.system.read(&spec.dest).await?,
            None => None,
        };
        let mut perms = hostkit::effective_perms(&spec.perms, existing.as_ref());
        if perms.mode.is_none() {
            perms.mode = Some(archive_mode.filter(|m| m & 0o111 != 0).unwrap_or(0o755));
        }
        cx.system.write_atomic(&spec.dest, &binary, &perms).await?;
        self.verify(cx, spec, &resolved, previous, &perms).await?;
        Ok(hostkit::written(
            format!(
                "installed {} {} to {}",
                spec.repo,
                resolved.version,
                spec.dest.display()
            ),
            &spec.dest,
            Digest::of(&binary),
            None,
            true,
        ))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "delete {} if it is unchanged since Buckle installed it",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        hostkit::remove_recorded_file(cx, record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::{FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const API: &str = "https://api.github.com/repos/acme/tool/releases";
    const DL: &str = "https://github.com/acme/tool/releases/download";

    fn sha(bytes: &[u8]) -> String {
        Digest::of(bytes).as_str().to_owned()
    }

    fn json_response(
        value: serde_json::Value,
    ) -> impl Fn(&HttpRequest) -> HttpResponse + Send + Sync {
        move |_| HttpResponse {
            status: 200,
            headers: vec![],
            body: value.to_string().into_bytes(),
        }
    }

    fn release(tag: &str, assets: &[(&str, &[u8])]) -> serde_json::Value {
        json!({
            "tag_name": tag,
            "assets": assets.iter().map(|(name, _)| json!({
                "name": name,
                "browser_download_url": format!("{DL}/{tag}/{name}"),
                "url": format!("https://api.github.com/assets/{name}"),
            })).collect::<Vec<_>>()
        })
    }

    /// A host whose `tool --version` reflects what is installed at /usr/local/bin/tool.
    fn host(arch: &str, installed: Option<&str>) -> FakeSystem {
        let sys = FakeSystem::new().on_command_ok("uname", &["-m"], &format!("{arch}\n"));
        let sys = match installed {
            Some(v) => sys.with_file(
                "/usr/local/bin/tool",
                format!("binary {v}"),
                FilePerms::mode(0o755),
            ),
            None => sys,
        };
        let probe = sys.clone();
        sys.on_command("tool", &["--version"], move |_| {
            match probe.file_string("/usr/local/bin/tool") {
                // A build the host's libc cannot load.
                Some(content) if content.contains("broken") => buckle_core::system::Output {
                    code: Some(1),
                    stdout: vec![],
                    stderr: GLIBC_ERROR.as_bytes().to_vec(),
                    timed_out: false,
                },
                Some(content) => {
                    ok_output(&format!("tool {}\n", content.trim_start_matches("binary ")))
                }
                None => buckle_core::system::Output {
                    code: Some(127),
                    stdout: vec![],
                    stderr: vec![],
                    timed_out: false,
                },
            }
        })
    }

    const GLIBC_ERROR: &str = "tool: /lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found (required by tool)\n";

    fn with_release(sys: FakeSystem, tag: &str) -> FakeSystem {
        let bin = format!("binary {}", tag.trim_start_matches('v'));
        with_release_binary(sys, tag, &bin)
    }

    fn with_release_binary(sys: FakeSystem, tag: &str, bin: &str) -> FakeSystem {
        let archive = tar_gz(&[
            ("tool-dir/", "", 0o755),
            ("tool-dir/tool", bin, 0o755),
            ("tool-dir/README", "r", 0o644),
        ]);
        let name = format!("tool_{}_linux_amd64.tar.gz", tag.trim_start_matches('v'));
        let sums = format!("{}  {name}\n", sha(&archive));
        let sums_name = "SHA256SUMS";
        let rel = release(tag, &[(&name, &archive), (sums_name, sums.as_bytes())]);
        let body = archive.clone();
        sys.on_http(&format!("{API}/latest"), json_response(rel.clone()))
            .on_http(&format!("{API}/tags/{tag}"), json_response(rel))
            .on_http(&format!("{DL}/{tag}/{name}"), move |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: body.clone(),
            })
            .on_http_ok(&format!("{DL}/{tag}/{sums_name}"), &sums)
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({
            "repo": "acme/tool",
            "asset": "tool_{version}_{os}_{arch}.tar.gz",
            "extract": "tool",
            "dest": "/usr/local/bin/tool",
            "probe": ["tool", "--version"],
        });
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[rstest]
    #[case::not_installed(None, Class::Change)]
    #[case::older(Some("1.2.3"), Class::Change)]
    #[case::current(Some("1.2.4"), Class::NoOp)]
    #[tokio::test]
    async fn latest_is_compared_with_the_probe(
        #[case] installed: Option<&str>,
        #[case] expected: Class,
    ) {
        let sys = with_release(host("x86_64", installed), "v1.2.4");
        assert_eq!(
            plan_class(&GithubRelease, &cx(&sys), &s(json!({}))).await,
            expected
        );
    }

    #[tokio::test]
    async fn updates_are_summarised_as_a_version_change() {
        let sys = with_release(host("x86_64", Some("1.2.3")), "v1.2.4");
        let planned = GithubRelease.plan(&cx(&sys), &s(json!({}))).await.unwrap();
        assert!(
            diff(&planned).summary().contains("1.2.3 → 1.2.4"),
            "{planned:?}"
        );
    }

    #[tokio::test]
    async fn install_extracts_the_binary_and_replans_as_noop() {
        let sys = with_release(host("x86_64", None), "v1.2.4");
        let c = cx(&sys);
        let spec = s(json!({"checksum_asset": "SHA256SUMS"}));
        let applied = apply(&GithubRelease, &c, &spec).await;
        assert!(applied.changed);
        assert_eq!(
            sys.file_string("/usr/local/bin/tool").unwrap(),
            "binary 1.2.4"
        );
        let meta =
            buckle_core::system::System::stat(&sys, std::path::Path::new("/usr/local/bin/tool"))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(meta.mode, Some(0o755));
        assert_eq!(plan_class(&GithubRelease, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn upgrade_replaces_the_old_binary() {
        let sys = with_release(host("x86_64", Some("1.2.3")), "v1.2.4");
        let c = cx(&sys);
        apply(&GithubRelease, &c, &s(json!({"mode": "0750"}))).await;
        assert_eq!(
            sys.file_string("/usr/local/bin/tool").unwrap(),
            "binary 1.2.4"
        );
        let meta =
            buckle_core::system::System::stat(&sys, std::path::Path::new("/usr/local/bin/tool"))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(meta.mode, Some(0o750));
    }

    #[tokio::test]
    async fn a_pinned_version_that_is_already_installed_makes_no_api_calls() {
        let sys = with_release(host("x86_64", Some("1.2.3")), "v1.2.3");
        let planned = GithubRelease
            .plan(&cx(&sys), &s(json!({"version": "v1.2.3"})))
            .await
            .unwrap();
        assert_eq!(planned, Planned::NoOp);
        assert!(sys.http_requests().is_empty());
    }

    #[rstest]
    #[case::with_v("v1.2.4")]
    #[case::without_v("1.2.4")]
    #[tokio::test]
    async fn pinned_versions_resolve_the_tag_either_way(#[case] version: &str) {
        let sys = with_release(host("x86_64", None), "v1.2.4");
        let c = cx(&sys);
        apply(&GithubRelease, &c, &s(json!({"version": version}))).await;
        assert_eq!(
            sys.file_string("/usr/local/bin/tool").unwrap(),
            "binary 1.2.4"
        );
        assert!(
            sys.http_requests()
                .iter()
                .any(|r| r.url.contains("/releases/tags/"))
        );
    }

    #[tokio::test]
    async fn architectures_map_through_the_defaults_and_the_task_table() {
        // aarch64 maps to arm64 by default.
        let sys = host("aarch64", None);
        let archive = tar_gz(&[("tool", "arm binary 1.2.4", 0o755)]);
        let rel = release("v1.2.4", &[("tool_1.2.4_linux_arm64.tar.gz", &archive)]);
        let sys = sys
            .on_http(&format!("{API}/latest"), json_response(rel))
            .on_http(
                &format!("{DL}/v1.2.4/tool_1.2.4_linux_arm64.tar.gz"),
                move |_| HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: archive.clone(),
                },
            );
        let c = cx(&sys);
        apply(&GithubRelease, &c, &s(json!({}))).await;
        assert_eq!(
            sys.file_string("/usr/local/bin/tool").unwrap(),
            "arm binary 1.2.4"
        );

        // armv7l has no default: blocked, and not an error.
        let sys = with_release(host("armv7l", None), "v1.2.4");
        let planned = GithubRelease.plan(&cx(&sys), &s(json!({}))).await.unwrap();
        let Planned::Blocked { reason } = &planned else {
            panic!("{planned:?}")
        };
        assert!(reason.contains("armv7l"), "{reason}");
        assert!(
            sys.http_requests().is_empty(),
            "no API call for an unsupported host"
        );

        // ...until the author maps it.
        let sys = host("armv7l", None);
        let archive = tar_gz(&[("tool", "v7 binary", 0o755)]);
        let rel = release("v1.2.4", &[("tool_1.2.4_linux_armv7.tar.gz", &archive)]);
        let sys = sys
            .on_http(&format!("{API}/latest"), json_response(rel))
            .on_http(&format!("{DL}/v1.2.4/"), move |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: archive.clone(),
            });
        let spec = s(json!({"arch_map": {"armv7l": "armv7"}}));
        assert!(
            GithubRelease
                .plan(&cx(&sys), &spec)
                .await
                .unwrap()
                .is_change()
        );
    }

    #[tokio::test]
    async fn the_arch_comes_from_facts_when_available() {
        use buckle_core::selectors::{HostContext, HostIdentity};
        let sys = with_release(
            FakeSystem::new().on_command_ok("tool", &["--version"], ""),
            "v1.2.4",
        );
        let host = HostContext::new(HostIdentity {
            id: "h".into(),
            ..Default::default()
        })
        .with_facts(json!({"hardware": {"arch": "x86_64"}}));
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .host(host)
            .build();
        assert!(
            GithubRelease
                .plan(&c, &s(json!({})))
                .await
                .unwrap()
                .is_change()
        );
        assert!(!sys.command_lines().iter().any(|l| l.starts_with("uname")));
    }

    #[tokio::test]
    async fn plain_binary_assets_are_installed_executable_and_checksummed() {
        let bin = b"#!/bin/sh\necho tool 2.0.0\n".to_vec();
        let sums = format!("{}  tool-linux-amd64\n", sha(&bin));
        let sys = host("x86_64", None);
        let rel = release(
            "v2.0.0",
            &[
                ("tool-linux-amd64", &bin),
                ("tool-linux-amd64.sha256", sums.as_bytes()),
            ],
        );
        let body = bin.clone();
        let sys = sys
            .on_http(&format!("{API}/latest"), json_response(rel))
            .on_http(&format!("{DL}/v2.0.0/tool-linux-amd64"), move |_| {
                HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: body.clone(),
                }
            })
            .on_http_ok(&format!("{DL}/v2.0.0/tool-linux-amd64.sha256"), &sums);
        let spec = s(
            json!({"asset": "tool-{os}-{arch}", "extract": null, "checksum_asset": "tool-{os}-{arch}.sha256"}),
        );
        let c = cx(&sys);
        apply(&GithubRelease, &c, &spec).await;
        assert_eq!(sys.file("/usr/local/bin/tool").unwrap(), bin);
        let meta =
            buckle_core::system::System::stat(&sys, std::path::Path::new("/usr/local/bin/tool"))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(meta.mode, Some(0o755));
    }

    #[tokio::test]
    async fn a_checksum_mismatch_writes_nothing() {
        let sys = with_release(host("x86_64", None), "v1.2.4");
        // Serve a different archive than the one the checksum describes.
        let tampered = tar_gz(&[("tool", "tampered", 0o755)]);
        let sys = sys.on_http(
            &format!("{DL}/v1.2.4/tool_1.2.4_linux_amd64.tar.gz"),
            move |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: tampered.clone(),
            },
        );
        let c = cx(&sys);
        let spec = s(json!({"checksum_asset": "SHA256SUMS"}));
        let err = GithubRelease
            .apply(&c, &spec, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("does not match its checksum"));
        assert!(!sys.exists("/usr/local/bin/tool"));
    }

    #[tokio::test]
    async fn misconfiguration_is_reported_with_advice() {
        let sys = with_release(host("x86_64", None), "v1.2.4");
        let c = cx(&sys);
        // Archive without `extract`.
        let err = GithubRelease
            .apply(&c, &s(json!({"extract": null})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(
            err.description().contains("`extract`"),
            "{}",
            err.description()
        );
        // Missing asset lists the alternatives.
        let err = GithubRelease
            .plan(&c, &s(json!({"asset": "nope-{version}.zip"})))
            .await
            .unwrap_err();
        assert!(
            err.description().contains("tool_1.2.4_linux_amd64.tar.gz"),
            "{}",
            err.description()
        );
        // Bad repo.
        assert!(
            GithubRelease
                .plan(&c, &s(json!({"repo": "justaname"})))
                .await
                .is_err()
        );
        // Binary not in the archive.
        let err = GithubRelease
            .apply(&c, &s(json!({"extract": "other"})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("`other` is not in"));
    }

    #[tokio::test]
    async fn the_token_is_sent_as_a_header_and_never_recorded() {
        const TOKEN: &str = "ghp_supersecrettokenvalue";
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = seen.clone();
        let base = with_release(host("x86_64", None).with_env("GH_TOKEN", TOKEN), "v1.2.4");
        let sys = base.on_http(&format!("{API}/latest"), move |req| {
            sink.lock().unwrap().extend(
                req.headers
                    .iter()
                    .filter(|(k, _)| k == "Authorization")
                    .map(|(_, v)| v.clone()),
            );
            HttpResponse {
                status: 200,
                headers: vec![],
                body: release("v1.2.4", &[("tool_1.2.4_linux_amd64.tar.gz", b"")])
                    .to_string()
                    .into_bytes(),
            }
        });
        let archive = tar_gz(&[("tool", "binary 1.2.4", 0o755)]);
        let sys = sys.on_http("https://api.github.com/assets/", move |_| HttpResponse {
            status: 200,
            headers: vec![],
            body: archive.clone(),
        });
        let c = cx(&sys);
        let spec = s(json!({"token_env": "GH_TOKEN"}));
        let planned = GithubRelease.plan(&c, &spec).await.unwrap();
        let applied = GithubRelease.apply(&c, &spec, &planned).await.unwrap();
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|h| *h == format!("Bearer {TOKEN}"))
        );
        assert!(
            sys.http_requests()
                .iter()
                .filter(|r| r.url.starts_with("https://api.github.com/assets/"))
                .all(|r| r
                    .headers
                    .iter()
                    .any(|(k, v)| k == "Authorization" && v.contains(TOKEN)))
        );
        let everything = serialised(&planned, &applied);
        assert!(!everything.contains(TOKEN), "{everything}");

        // An unset variable is a clear user error that does not guess.
        let sys = with_release(host("x86_64", None), "v1.2.4");
        let err = GithubRelease.plan(&cx(&sys), &spec).await.unwrap_err();
        assert!(err.description().contains("GH_TOKEN"));
    }

    #[tokio::test]
    async fn without_a_probe_an_existing_binary_counts_as_installed() {
        let sys = with_release(host("x86_64", Some("0.0.1")), "v1.2.4");
        let spec = s(json!({"probe": null}));
        assert_eq!(
            plan_class(&GithubRelease, &cx(&sys), &spec).await,
            Class::NoOp
        );
        let sys = with_release(host("x86_64", None), "v1.2.4");
        assert_eq!(
            plan_class(&GithubRelease, &cx(&sys), &spec).await,
            Class::Change
        );
    }

    #[tokio::test]
    async fn probe_regex_selects_the_version() {
        let sys = FakeSystem::new()
            .on_command_ok("uname", &["-m"], "x86_64")
            .on_command_ok("tool", &["--version"], "tool build 7 (release 1.2.4) abc")
            .with_file("/usr/local/bin/tool", "x", FilePerms::mode(0o755));
        let sys = with_release(sys, "v1.2.4");
        let ok = s(json!({"probe_regex": r"release (\S+)\)"}));
        assert_eq!(
            plan_class(&GithubRelease, &cx(&sys), &ok).await,
            Class::NoOp
        );
        let stale = s(json!({"probe_regex": r"build (\d+)"}));
        assert_eq!(
            plan_class(&GithubRelease, &cx(&sys), &stale).await,
            Class::Change
        );
    }

    #[tokio::test]
    async fn a_binary_that_does_not_run_plans_a_reinstall_with_the_reason() {
        let sys = with_release(host("x86_64", Some("broken 1.2.4")), "v1.2.4");
        let planned = GithubRelease.plan(&cx(&sys), &s(json!({}))).await.unwrap();
        let summary = diff(&planned).summary().to_owned();
        assert!(summary.contains("reinstall"), "{summary}");
        assert!(summary.contains("GLIBC_2.39"), "{summary}");
    }

    #[rstest]
    #[case::fresh_install(None, None)]
    #[case::upgrade(Some("1.2.3"), Some("binary 1.2.3"))]
    #[case::reinstall(Some("broken 1.2.4"), Some("binary broken 1.2.4"))]
    #[tokio::test]
    async fn a_release_that_does_not_run_fails_and_keeps_the_previous_binary(
        #[case] installed: Option<&str>,
        #[case] kept: Option<&str>,
    ) {
        let sys = with_release_binary(host("x86_64", installed), "v1.2.4", "broken 1.2.4");
        let err = GithubRelease
            .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(
            err.description().contains("GLIBC_2.39"),
            "{}",
            err.description()
        );
        assert_eq!(sys.file_string("/usr/local/bin/tool").as_deref(), kept);
    }

    #[tokio::test]
    async fn removal_respects_drift() {
        let sys = with_release(host("x86_64", None), "v1.2.4");
        let c = cx(&sys);
        let rec = record(
            "github.release",
            "/usr/local/bin/tool",
            &apply(&GithubRelease, &c, &s(json!({}))).await,
        );
        put(&sys, "/usr/local/bin/tool", "hand built").await;
        assert!(!GithubRelease.remove(&c, &rec).await.unwrap().changed);
        put(&sys, "/usr/local/bin/tool", "binary 1.2.4").await;
        assert!(GithubRelease.remove(&c, &rec).await.unwrap().changed);
    }
}
