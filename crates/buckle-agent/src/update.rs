//! Self-update from GitHub releases (design §08, "Self-update").
//!
//! Buckle's release assets are named `buckle-<os>-<arch>[.exe]` (Go-style
//! names, see `.github/workflows/release.yml`) under `v<semver>` tags of
//! `SierraSoftworks/buckle`. [`update_rs::GitHubSource`] lists the releases,
//! selects the asset for this platform and verifies the download against the
//! SHA-256 digest GitHub reports for it.
//!
//! update-rs' own [`update_rs::UpdateManager`] swaps binaries with a
//! three-phase relaunch that needs a hook in `main()` and leaves detached
//! processes behind, which a service manager's cgroup would kill. Buckle
//! instead downloads the new binary next to the running one and renames it
//! into place (atomic on Unix; on Windows the running image is first moved
//! aside), then the caller exits so systemd, launchd or the SCM restarts it.
//! Downloads without a reported digest are refused.

use async_trait::async_trait;
use buckle_core::error::HumanizableResult;
use std::path::PathBuf;
use update_rs::{GitHubSource, Release, Source};

/// The repository releases are published from.
pub const RELEASE_REPOSITORY: &str = "SierraSoftworks/buckle";

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// A new binary is in place; restart to run it.
    Updated {
        /// The version installed.
        version: String,
    },
    /// Already on the requested (or latest) version.
    UpToDate {
        /// The current version.
        version: String,
    },
}

/// Replaces the running binary with another release.
#[async_trait]
pub trait Updater: Send + Sync + 'static {
    /// Installs `version` (or the latest release when `None`).
    async fn update(&self, version: Option<&str>) -> buckle_core::Result<UpdateOutcome>;
}

/// Updates from GitHub releases.
pub struct GitHubUpdater {
    source: GitHubSource,
    current: String,
    target: Option<PathBuf>,
}

impl GitHubUpdater {
    /// An updater for this platform's release asset; `current` is the running version.
    pub fn new(current: impl Into<String>) -> Self {
        Self {
            source: GitHubSource::new(RELEASE_REPOSITORY, update_rs::naming::go("buckle"))
                .with_release_tag_prefix("v"),
            current: current.into(),
            target: None,
        }
    }

    /// Uses other GitHub endpoints (for tests and GitHub Enterprise mirrors).
    pub fn with_endpoints(mut self, web: &str, api: &str) -> Self {
        self.source = self.source.with_github_endpoints(web, api);
        self
    }

    /// Uses another asset name pattern.
    pub fn with_asset_pattern(mut self, pattern: &str, web: &str, api: &str) -> Self {
        self.source = GitHubSource::new(RELEASE_REPOSITORY, pattern)
            .with_release_tag_prefix("v")
            .with_github_endpoints(web, api);
        self
    }

    /// Replaces this file instead of the running executable.
    pub fn with_target(mut self, target: impl Into<PathBuf>) -> Self {
        self.target = Some(target.into());
        self
    }

    fn target(&self) -> buckle_core::Result<PathBuf> {
        match &self.target {
            Some(t) => Ok(t.clone()),
            None => std::env::current_exe().map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    "Buckle could not determine the path of its own executable.",
                    &["Update Buckle by downloading the release manually."],
                )
            }),
        }
    }
}

/// Picks the release to install: `wanted` exactly (with or without a `v`), or
/// the newest non-prerelease that has an asset for this platform.
pub fn choose_release<'a>(releases: &'a [Release], wanted: Option<&str>) -> Option<&'a Release> {
    match wanted {
        Some(w) => {
            let w = w.trim_start_matches('v');
            releases
                .iter()
                .find(|r| r.version.to_string() == w || r.id.trim_start_matches('v') == w)
        }
        None => Release::get_latest(
            releases
                .iter()
                .filter(|r| !r.prerelease && r.get_variant().is_some()),
        ),
    }
}

#[async_trait]
impl Updater for GitHubUpdater {
    async fn update(&self, version: Option<&str>) -> buckle_core::Result<UpdateOutcome> {
        // A span guard held across awaits would attach whatever else runs on
        // this thread to the update span; instrument the future instead.
        let span = tracing::info_span!("agent.update", version = version.unwrap_or("latest"));
        tracing::Instrument::instrument(self.update_in_span(version), span).await
    }
}

impl GitHubUpdater {
    async fn update_in_span(&self, version: Option<&str>) -> buckle_core::Result<UpdateOutcome> {
        let releases = self.source.get_releases().await?;
        let release = choose_release(&releases, version).ok_or_else(|| {
            buckle_core::error::user(
                match version {
                    Some(v) => format!("There is no Buckle release {v} on GitHub."),
                    None => "There is no Buckle release for this platform on GitHub.".to_owned(),
                },
                &["Check the version against https://github.com/SierraSoftworks/buckle/releases."],
            )
        })?;
        let current = self.current.trim_start_matches('v');
        let not_newer = version.is_none()
            && current
                .parse::<semver::Version>()
                .is_ok_and(|c| release.version <= c);
        if release.version.to_string() == current || not_newer {
            return Ok(UpdateOutcome::UpToDate {
                version: current.to_owned(),
            });
        }
        let variant = release.get_variant().ok_or_else(|| {
            buckle_core::error::user(
                format!(
                    "Release {} has no binary for this platform ({}).",
                    release.id,
                    update_rs::naming::go("buckle")
                ),
                &["Install Buckle for this platform from source, or pick another release."],
            )
        })?;
        if variant.sha256.is_none() {
            return Err(buckle_core::error::user(
                format!(
                    "GitHub reports no SHA-256 digest for {} in release {}, so the download cannot be verified.",
                    variant.name, release.id
                ),
                &["Download and verify the release manually."],
            ));
        }

        let target = self.target()?;
        let staging = target.with_file_name(format!(
            ".{}.update-{}",
            target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "buckle".into()),
            release.version
        ));
        let mut file = std::fs::File::create(&staging).to_human_error()?;
        if let Err(err) = self.source.get_binary(release, variant, &mut file).await {
            drop(file);
            let _ = std::fs::remove_file(&staging);
            return Err(err);
        }
        file.sync_all().to_human_error()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
                .to_human_error()?;
        }
        // Windows cannot replace a running executable, but it can rename it:
        // move it aside first, and put it back if the swap fails so the
        // service still has a binary to start.
        #[cfg(windows)]
        let aside = {
            let aside = target.with_extension("old.exe");
            let _ = std::fs::remove_file(&aside);
            std::fs::rename(&target, &aside).to_human_error()?;
            aside
        };
        if let Err(err) = std::fs::rename(&staging, &target) {
            let _ = std::fs::remove_file(&staging);
            #[cfg(windows)]
            if let Err(restore) = std::fs::rename(&aside, &target) {
                tracing::error!(error = %restore, "could not restore the previous binary after a failed update");
            }
            return Err(err).to_human_error();
        }
        tracing::info!(version = %release.version, path = %target.display(), "installed a new Buckle binary");
        Ok(UpdateOutcome::Updated {
            version: release.version.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ASSET: &str = "buckle-test-asset";

    fn releases_json(digest: Option<&str>) -> String {
        let digest_field = digest
            .map(|d| format!(r#", "digest": "sha256:{d}""#))
            .unwrap_or_default();
        format!(
            r#"[
              {{"name":"2.1.0","tag_name":"v2.1.0","body":"","prerelease":false,"assets":[{{"name":"{ASSET}"{digest_field}}}]}},
              {{"name":"2.0.0","tag_name":"v2.0.0","body":"","prerelease":false,"assets":[{{"name":"{ASSET}"{digest_field}}}]}},
              {{"name":"2.2.0-beta.1","tag_name":"v2.2.0-beta.1","body":"","prerelease":true,"assets":[{{"name":"{ASSET}"{digest_field}}}]}}
            ]"#
        )
    }

    async fn server(binary: &[u8], digest: Option<&str>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{RELEASE_REPOSITORY}/releases")))
            .respond_with(ResponseTemplate::new(200).set_body_string(releases_json(digest)))
            .mount(&server)
            .await;
        for tag in ["v2.1.0", "v2.0.0"] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/{RELEASE_REPOSITORY}/releases/download/{tag}/{ASSET}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(binary.to_vec()))
                .mount(&server)
                .await;
        }
        server
    }

    fn updater(server: &MockServer, current: &str, target: &std::path::Path) -> GitHubUpdater {
        GitHubUpdater::new(current)
            .with_asset_pattern(ASSET, &server.uri(), &server.uri())
            .with_target(target)
    }

    #[tokio::test]
    async fn installs_the_latest_stable_release_over_the_target() {
        let binary = b"#!new buckle";
        let digest = hex::encode(sha2::Sha256::digest(binary));
        let server = server(binary, Some(&digest)).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("buckle");
        std::fs::write(&target, b"old").expect("write");

        let outcome = updater(&server, "2.0.0", &target)
            .update(None)
            .await
            .expect("update");
        assert_eq!(
            outcome,
            UpdateOutcome::Updated {
                version: "2.1.0".into()
            }
        );
        assert_eq!(std::fs::read(&target).expect("read"), binary);

        let again = updater(&server, "2.1.0", &target)
            .update(None)
            .await
            .expect("update");
        assert_eq!(
            again,
            UpdateOutcome::UpToDate {
                version: "2.1.0".into()
            }
        );

        let pinned = updater(&server, "2.1.0", &target)
            .update(Some("v2.0.0"))
            .await
            .expect("update");
        assert_eq!(
            pinned,
            UpdateOutcome::Updated {
                version: "2.0.0".into()
            }
        );
    }

    #[tokio::test]
    async fn refuses_a_download_that_does_not_match_its_digest() {
        let digest = hex::encode(sha2::Sha256::digest(b"what github says"));
        let server = server(b"tampered", Some(&digest)).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("buckle");
        std::fs::write(&target, b"old").expect("write");
        assert!(
            updater(&server, "2.0.0", &target)
                .update(None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"old");
        let leftovers = std::fs::read_dir(dir.path()).expect("list").count();
        assert_eq!(leftovers, 1, "the staging file is removed");
    }

    #[tokio::test]
    async fn refuses_a_download_without_a_digest() {
        let server = server(b"bin", None).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("buckle");
        std::fs::write(&target, b"old").expect("write");
        assert!(
            updater(&server, "2.0.0", &target)
                .update(None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"old");
    }
}
