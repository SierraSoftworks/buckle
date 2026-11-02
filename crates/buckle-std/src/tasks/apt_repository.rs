//! `apt.repository`: a deb822 apt source plus its signing key.

use super::apt_common::apt_get;
use super::sys_exec::{file_matches_digest, run_checked};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::FileSink;
use buckle_core::secrets::{Redacted, Rendered, SecretField};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FilePerms, HttpRequest};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Adds an apt repository as a deb822 `.sources` file and downloads its signing key.
///
/// Writes `/etc/apt/sources.list.d/<name>.sources` and, for `signed_by.url`
/// (an ASCII-armoured key) or `signed_by.keyring_url` (a binary keyring),
/// downloads the key to `/usr/share/keyrings/<name>.asc` or `<name>.gpg` and
/// references it in `Signed-By`. `signed_by.path` points at a keyring that is
/// already on the host and downloads nothing. Without `signed_by` the
/// repository is added unsigned-by-key, which apt only accepts for sources it
/// can verify another way.
///
/// `uri` (one) or `uris` (several) give the mirrors, `suites` the
/// distributions (`bookworm`, `stable`, or a flat-repository path ending in
/// `/`), and `components` default to `main`. `types` defaults to `[deb]`.
/// `enabled: false` writes the file with `Enabled: no`.
///
/// `auth` (a `login` and a `password`, which may reference secrets) writes
/// `/etc/apt/auth.conf.d/<name>.conf` (mode 0600) with a `machine` line for
/// each URI, for private repositories; its content never appears in plans.
///
/// Plans diff the rendered sources file; a missing key file is also a change
/// (the key's contents are not compared). Applying runs `apt-get update` once.
/// Removal deletes both files (leaving the sources file in place if it was
/// edited by hand since Buckle wrote it) and runs `apt-get update`.
pub struct AptRepository;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// File name stem: `/etc/apt/sources.list.d/<name>.sources`.
    pub name: String,
    /// A single repository URI.
    #[serde(default)]
    pub uri: Option<String>,
    /// Several mirror URIs for the same repository.
    #[serde(default)]
    pub uris: Vec<String>,
    /// Suites (distributions), e.g. `bookworm`.
    pub suites: Vec<String>,
    /// Components; defaults to `[main]`.
    #[serde(default = "default_components")]
    pub components: Vec<String>,
    /// Where the signing key comes from.
    #[serde(default)]
    pub signed_by: Option<SignedBy>,
    /// Restrict to these architectures (`amd64`, `arm64`).
    #[serde(default)]
    pub architectures: Option<Vec<String>>,
    /// Archive types; defaults to `[deb]`.
    #[serde(default = "default_types")]
    pub types: Vec<String>,
    /// Write the source disabled (`Enabled: no`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Credentials for a private repository, written to apt's auth.conf.d.
    #[serde(default)]
    pub auth: Option<Auth>,
}

/// `machine`/`login`/`password` for `/etc/apt/auth.conf.d/<name>.conf`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// The user name (or token name) apt sends.
    pub login: String,
    /// The password or token; may reference secrets (`{{ secrets.repo_token }}`).
    pub password: SecretField,
}

fn default_components() -> Vec<String> {
    vec!["main".into()]
}

fn default_types() -> Vec<String> {
    vec!["deb".into()]
}

fn default_true() -> bool {
    true
}

/// Exactly one of `url`, `keyring_url` or `path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignedBy {
    /// URL of an ASCII-armoured public key, saved as `/usr/share/keyrings/<name>.asc`.
    #[serde(default)]
    pub url: Option<String>,
    /// URL of a binary keyring, saved as `/usr/share/keyrings/<name>.gpg`.
    #[serde(default)]
    pub keyring_url: Option<String>,
    /// A keyring file that already exists on the host.
    #[serde(default)]
    pub path: Option<String>,
}

/// Where the key ends up and where it comes from.
struct Key {
    /// Path referenced by `Signed-By`.
    path: String,
    /// Download source, when Buckle fetches it.
    url: Option<String>,
}

impl Spec {
    fn invalid(msg: String) -> buckle_core::Error {
        buckle_core::error::user(
            format!("apt.repository: {msg}"),
            &["See `buckle catalog apt.repository` for the fields this task accepts."],
        )
    }

    fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
        {
            return Err(Self::invalid(format!(
                "`name` '{}' must be a file name stem (letters, digits, '-', '_', '.', '+').",
                self.name
            )));
        }
        if self.all_uris().is_empty() {
            return Err(Self::invalid("set `uri` or `uris`.".into()));
        }
        if self.suites.is_empty() {
            return Err(Self::invalid("set `suites`.".into()));
        }
        if let Some(s) = &self.signed_by
            && [&s.url, &s.keyring_url, &s.path]
                .iter()
                .filter(|v| v.is_some())
                .count()
                != 1
        {
            return Err(Self::invalid(
                "`signed_by` needs exactly one of `url`, `keyring_url` or `path`.".into(),
            ));
        }
        Ok(())
    }

    fn all_uris(&self) -> Vec<&str> {
        self.uri
            .iter()
            .chain(self.uris.iter())
            .map(String::as_str)
            .collect()
    }

    fn sources_path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/apt/sources.list.d/{}.sources", self.name))
    }

    fn key(&self) -> Option<Key> {
        let s = self.signed_by.as_ref()?;
        if let Some(url) = &s.url {
            return Some(Key {
                path: format!("/usr/share/keyrings/{}.asc", self.name),
                url: Some(url.clone()),
            });
        }
        if let Some(url) = &s.keyring_url {
            return Some(Key {
                path: format!("/usr/share/keyrings/{}.gpg", self.name),
                url: Some(url.clone()),
            });
        }
        s.path.as_ref().map(|p| Key {
            path: p.clone(),
            url: None,
        })
    }

    fn auth_path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/apt/auth.conf.d/{}.conf", self.name))
    }

    /// The auth.conf content (real values, through the secret sink).
    fn render_auth(&self, cx: &Cx) -> Result<Option<Rendered>> {
        let Some(auth) = &self.auth else {
            return Ok(None);
        };
        if auth.login.contains(char::is_whitespace) {
            return Err(Self::invalid(
                "`auth.login` may not contain whitespace.".into(),
            ));
        }
        // One template so the secret is only ever revealed by the file sink.
        let mut template = String::from("# Managed by Buckle.\n");
        for uri in self.all_uris() {
            let machine = uri
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(uri)
                .trim_end_matches('/');
            template.push_str(&format!(
                "machine {machine} login {} password {}\n",
                auth.login,
                auth.password.raw()
            ));
        }
        SecretField::new(template)
            .resolve(&cx.templater, &cx.host)
            .map(Some)
    }

    /// The deb822 stanza Buckle writes.
    fn render(&self) -> String {
        let mut out = String::from("# Managed by Buckle. Local changes will be overwritten.\n");
        out.push_str(&format!("Types: {}\n", self.types.join(" ")));
        out.push_str(&format!("URIs: {}\n", self.all_uris().join(" ")));
        out.push_str(&format!("Suites: {}\n", self.suites.join(" ")));
        // Flat repositories (suites ending in '/') have no components.
        if !self.components.is_empty() && !self.suites.iter().all(|s| s.ends_with('/')) {
            out.push_str(&format!("Components: {}\n", self.components.join(" ")));
        }
        if let Some(archs) = &self.architectures
            && !archs.is_empty()
        {
            out.push_str(&format!("Architectures: {}\n", archs.join(" ")));
        }
        if let Some(key) = self.key() {
            out.push_str(&format!("Signed-By: {}\n", key.path));
        }
        if !self.enabled {
            out.push_str("Enabled: no\n");
        }
        out
    }
}

#[async_trait]
impl Task for AptRepository {
    type Spec = Spec;
    const KIND: &'static str = "apt.repository";
    const LOCKS: &'static [&'static str] = &["apt"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Add an apt repository (deb822 .sources file) and download its signing key.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!(
            "apt repository {} ({})",
            spec.name,
            spec.all_uris().join(", ")
        )
    }

    #[tracing::instrument(name = "task.apt.repository.plan", skip_all, fields(repository = %spec.name))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        spec.validate()?;
        if let Some(blocked) = super::apt_common::unavailable(cx, false).await? {
            return Ok(blocked);
        }
        let wanted = spec.render();
        let path = spec.sources_path();
        let current = cx
            .system
            .read(&path)
            .await?
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        let key_missing = match spec.key() {
            Some(Key { path, url: Some(_) }) => {
                cx.system.stat(std::path::Path::new(&path)).await?.is_none()
            }
            _ => false,
        };
        let auth_changed = match spec.render_auth(cx)? {
            Some(rendered) => {
                file_matches_digest(cx, &spec.auth_path(), Some(&rendered.digest())).await?
                    != Some(true)
            }
            None => false,
        };
        let summary = format!("apt repository {} -> {}", spec.name, path.display());
        match current {
            None => Ok(Planned::change(Diff::Create {
                summary,
                preview: Some(Redacted::plain(wanted)),
            })),
            Some(current) if current != wanted => Ok(Planned::change(Diff::unified(
                summary,
                &Redacted::plain(current),
                &Redacted::plain(wanted),
            ))),
            Some(_) if key_missing => Ok(Planned::change(Diff::Opaque {
                summary: format!("download the signing key for apt repository {}", spec.name),
            })),
            Some(_) if auth_changed => Ok(Planned::change(Diff::Opaque {
                summary: format!(
                    "write the credentials for apt repository {} to {} (content not shown)",
                    spec.name,
                    spec.auth_path().display()
                ),
            })),
            Some(_) => Ok(Planned::NoOp),
        }
    }

    #[tracing::instrument(name = "task.apt.repository.apply", skip_all, fields(repository = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        spec.validate()?;
        let mut artefacts = Vec::new();
        if let Some(Key {
            path,
            url: Some(url),
        }) = spec.key()
        {
            let response = cx
                .system
                .http(HttpRequest::get(url.as_str()).timeout(Duration::from_secs(60)))
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(buckle_core::error::user(
                    format!(
                        "Downloading the signing key for apt repository '{}' from {url} returned HTTP {}.",
                        spec.name, response.status
                    ),
                    &["Check the `signed_by` URL; the vendor may have moved the key."],
                ));
            }
            cx.system
                .write_atomic(
                    std::path::Path::new(&path),
                    &response.body,
                    &FilePerms::mode(0o644),
                )
                .await?;
            artefacts.push(Artefact::File { path: path.into() });
        }
        if let Some(rendered) = spec.render_auth(cx)? {
            let path = spec.auth_path();
            if let Some(dir) = path.parent() {
                cx.system.mkdir_all(dir, &FilePerms::mode(0o755)).await?;
            }
            FileSink
                .write_rendered(&*cx.system, &path, &rendered, &FilePerms::mode(0o600))
                .await?;
            artefacts.push(Artefact::File { path });
        }
        let content = spec.render();
        let sources = spec.sources_path();
        cx.system
            .write_atomic(&sources, content.as_bytes(), &FilePerms::mode(0o644))
            .await?;
        run_checked(cx, apt_get("update", false)).await?;

        let mut applied = Applied::changed(format!("added apt repository {}", spec.name))
            .with_shadow(
                Redacted::plain(content.clone()),
                Digest::of(content.as_bytes()),
            );
        applied = applied.with_artefact(Artefact::File { path: sources });
        for a in artefacts {
            applied = applied.with_artefact(a);
        }
        Ok(applied.with_artefact(Artefact::Repository {
            manager: "apt".into(),
            name: spec.name.clone(),
        }))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let files: Vec<String> = file_artefacts(record)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!(
            "delete {}, then apt-get update",
            if files.is_empty() {
                record.key.key.clone()
            } else {
                files.join(" and ")
            }
        )
    }

    #[tracing::instrument(name = "task.apt.repository.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let files = file_artefacts(record);
        // A hand-edited sources file is left alone, together with the key it uses.
        for path in &files {
            if path.extension().is_some_and(|e| e == "sources")
                && file_matches_digest(cx, path, record.content_digest.as_ref()).await?
                    == Some(false)
            {
                return Ok(Applied::skipped(format!(
                    "{} was modified since Buckle wrote it; left in place",
                    path.display()
                )));
            }
        }
        let mut removed = Vec::new();
        for path in files {
            if cx.system.remove(&path, false).await? {
                removed.push(path);
            }
        }
        if removed.is_empty() {
            return Ok(Applied::unchanged(format!(
                "apt repository {} was already removed",
                record.key.key
            )));
        }
        run_checked(cx, apt_get("update", false)).await?;
        Ok(Applied::changed(format!(
            "removed apt repository {}",
            record.key.key
        )))
    }
}

fn file_artefacts(record: &ResourceRecord) -> Vec<PathBuf> {
    record
        .artefacts
        .iter()
        .filter_map(|a| match a {
            Artefact::File { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

buckle_core::register_task!(AptRepository);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    const KEY: &str =
        "-----BEGIN PGP PUBLIC KEY BLOCK-----\nmQINBFMO\n-----END PGP PUBLIC KEY BLOCK-----\n";

    fn host() -> FakeSystem {
        FakeSystem::new()
            .on_http("https://apt.releases.hashicorp.com/gpg", |_| HttpResponse {
                status: 200,
                headers: vec![],
                body: KEY.as_bytes().to_vec(),
            })
            .on_command_ok(
                "apt-get",
                &["update"],
                "Hit:1 https://apt.releases.hashicorp.com bookworm InRelease\n",
            )
    }

    fn hashicorp() -> serde_json::Value {
        json!({
            "name": "hashicorp",
            "uri": "https://apt.releases.hashicorp.com",
            "suites": ["bookworm"],
            "signed_by": {"url": "https://apt.releases.hashicorp.com/gpg"},
        })
    }

    #[rstest]
    #[case::minimal(
        hashicorp(),
        "# Managed by Buckle. Local changes will be overwritten.\nTypes: deb\nURIs: https://apt.releases.hashicorp.com\nSuites: bookworm\nComponents: main\nSigned-By: /usr/share/keyrings/hashicorp.asc\n"
    )]
    #[case::mirrors_arch_disabled(
        json!({"name": "docker", "uris": ["https://a.example/linux/debian", "https://b.example/linux/debian"], "suites": ["bookworm", "bookworm-updates"], "components": ["stable", "test"], "architectures": ["amd64", "arm64"], "types": ["deb", "deb-src"], "enabled": false, "signed_by": {"keyring_url": "https://a.example/docker.gpg"}}),
        "# Managed by Buckle. Local changes will be overwritten.\nTypes: deb deb-src\nURIs: https://a.example/linux/debian https://b.example/linux/debian\nSuites: bookworm bookworm-updates\nComponents: stable test\nArchitectures: amd64 arm64\nSigned-By: /usr/share/keyrings/docker.gpg\nEnabled: no\n"
    )]
    #[case::flat_repo_and_existing_keyring(
        json!({"name": "flat", "uri": "https://x.example/repo", "suites": ["./"], "signed_by": {"path": "/etc/apt/keyrings/x.gpg"}}),
        "# Managed by Buckle. Local changes will be overwritten.\nTypes: deb\nURIs: https://x.example/repo\nSuites: ./\nSigned-By: /etc/apt/keyrings/x.gpg\n"
    )]
    #[case::unsigned(
        json!({"name": "local", "uri": "http://localhost:3142/debian", "suites": ["bookworm"]}),
        "# Managed by Buckle. Local changes will be overwritten.\nTypes: deb\nURIs: http://localhost:3142/debian\nSuites: bookworm\nComponents: main\n"
    )]
    fn renders_deb822(#[case] spec_json: serde_json::Value, #[case] expected: &str) {
        let s: Spec = spec(spec_json);
        assert_eq!(s.render(), expected);
    }

    #[rstest]
    #[case(json!({"name": "a b", "uri": "x", "suites": ["s"]}))]
    #[case(json!({"name": "a", "suites": ["s"]}))]
    #[case(json!({"name": "a", "uri": "x", "suites": []}))]
    #[case(json!({"name": "a", "uri": "x", "suites": ["s"], "signed_by": {}}))]
    #[case(json!({"name": "a", "uri": "x", "suites": ["s"], "signed_by": {"url": "u", "path": "p"}}))]
    #[tokio::test]
    async fn invalid_specs_are_user_errors(#[case] spec_json: serde_json::Value) {
        let cx = cx(&host());
        let err = AptRepository.plan(&cx, &spec(spec_json)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User), "{err:?}");
    }

    #[tokio::test]
    async fn apply_writes_key_and_sources_updates_once_and_converges() {
        let sys = host();
        let cx = cx(&sys);
        let s: Spec = spec(hashicorp());

        let planned = AptRepository.plan(&cx, &s).await.unwrap();
        assert!(matches!(
            planned,
            Planned::Change {
                diff: Diff::Create { .. }
            }
        ));

        let applied = AptRepository.apply(&cx, &s, &planned).await.unwrap();
        assert!(applied.changed);
        assert_eq!(
            sys.file_string("/usr/share/keyrings/hashicorp.asc")
                .as_deref(),
            Some(KEY)
        );
        let sources = sys
            .file_string("/etc/apt/sources.list.d/hashicorp.sources")
            .unwrap();
        assert!(sources.contains("Signed-By: /usr/share/keyrings/hashicorp.asc"));
        let updates = sys
            .command_lines()
            .iter()
            .filter(|l| l.starts_with("apt-get update"))
            .count();
        assert_eq!(updates, 1);
        assert_eq!(sys.http_requests().len(), 1);
        assert!(applied.artefacts.contains(&Artefact::Repository {
            manager: "apt".into(),
            name: "hashicorp".into()
        }));
        assert_eq!(applied.artefacts.len(), 3);
        assert!(applied.content_digest.is_some());

        assert_eq!(AptRepository.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn plan_shows_a_diff_when_the_sources_file_differs() {
        let sys = host().with_file(
            "/etc/apt/sources.list.d/hashicorp.sources",
            "Types: deb\nURIs: https://old.example\nSuites: bookworm\n",
            FilePerms::mode(0o644),
        );
        let cx = cx(&sys);
        let planned = AptRepository.plan(&cx, &spec(hashicorp())).await.unwrap();
        match planned {
            Planned::Change {
                diff: Diff::Update {
                    unified: Some(u), ..
                },
            } => {
                assert!(u.get().contains("-URIs: https://old.example"));
                assert!(
                    u.get()
                        .contains("+URIs: https://apt.releases.hashicorp.com")
                );
            }
            other => panic!("expected an update diff, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_key_is_a_change_even_when_sources_match() {
        let s: Spec = spec(hashicorp());
        let sys = host().with_file(
            "/etc/apt/sources.list.d/hashicorp.sources",
            s.render(),
            FilePerms::mode(0o644),
        );
        let cx = cx(&sys);
        let planned = AptRepository.plan(&cx, &s).await.unwrap();
        assert!(matches!(
            planned,
            Planned::Change {
                diff: Diff::Opaque { .. }
            }
        ));
    }

    #[tokio::test]
    async fn existing_keyrings_are_not_downloaded() {
        let sys = host();
        let cx = cx(&sys);
        let s: Spec = spec(
            json!({"name": "x", "uri": "https://x.example", "suites": ["s"], "signed_by": {"path": "/etc/apt/keyrings/x.gpg"}}),
        );
        let applied = AptRepository.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(sys.http_requests().is_empty());
        assert_eq!(applied.artefacts.len(), 2);
    }

    #[tokio::test]
    async fn key_download_failures_name_the_repository() {
        let sys = FakeSystem::new().on_http("https://gone.example", |_| HttpResponse {
            status: 404,
            headers: vec![],
            body: vec![],
        });
        let cx = cx(&sys);
        let s: Spec = spec(
            json!({"name": "gone", "uri": "https://gone.example", "suites": ["s"], "signed_by": {"url": "https://gone.example/key"}}),
        );
        let err = AptRepository
            .apply(&cx, &s, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("'gone'") && err.description().contains("404"));
        assert!(!sys.exists("/etc/apt/sources.list.d/gone.sources"));
    }

    #[tokio::test]
    async fn removal_deletes_both_files_and_refreshes_the_cache() {
        let sys = host();
        let cx = cx(&sys);
        let s: Spec = spec(hashicorp());
        let applied = AptRepository.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let record = record_of("apt.repository", "hashicorp", &applied);
        assert_eq!(
            AptRepository.describe_removal(&record),
            "delete /etc/apt/sources.list.d/hashicorp.sources and /usr/share/keyrings/hashicorp.asc, then apt-get update"
        );
        let removed = AptRepository.remove(&cx, &record).await.unwrap();
        assert!(removed.changed);
        assert!(!sys.exists("/etc/apt/sources.list.d/hashicorp.sources"));
        assert!(!sys.exists("/usr/share/keyrings/hashicorp.asc"));
        let updates = sys
            .command_lines()
            .iter()
            .filter(|l| l.starts_with("apt-get update"))
            .count();
        assert_eq!(updates, 2, "one for apply, one for remove");
    }

    #[tokio::test]
    async fn removal_leaves_a_hand_edited_sources_file() {
        let sys = host();
        let cx = cx(&sys);
        let applied = AptRepository
            .apply(&cx, &spec(hashicorp()), &Planned::NoOp)
            .await
            .unwrap();
        let record = record_of("apt.repository", "hashicorp", &applied);
        sys.write_atomic(
            std::path::Path::new("/etc/apt/sources.list.d/hashicorp.sources"),
            b"Types: deb\n# edited by an operator\n",
            &FilePerms::default(),
        )
        .await
        .unwrap();
        let removed = AptRepository.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert!(removed.summary.contains("modified"));
        assert!(sys.exists("/etc/apt/sources.list.d/hashicorp.sources"));
        assert!(sys.exists("/usr/share/keyrings/hashicorp.asc"));
    }

    use buckle_core::system::System as _;

    #[tokio::test]
    async fn private_repository_credentials_go_to_auth_conf_and_never_into_plans() {
        let sys = host();
        let c = crate::tasks::hostkit::testkit::cx_secrets(
            &sys,
            buckle_core::testing::MemorySource::new(),
            crate::tasks::hostkit::testkit::secrets("repo_token", "tok-very-private"),
        );
        let mut raw = hashicorp();
        raw["auth"] = json!({"login": "ci", "password": "{{ secrets.repo_token }}"});
        let s: Spec = spec(raw);
        assert!(lookup_schema_marks_password());
        let planned = AptRepository.plan(&c, &s).await.unwrap();
        let applied = AptRepository.apply(&c, &s, &planned).await.unwrap();
        let text = sys
            .file_string("/etc/apt/auth.conf.d/hashicorp.conf")
            .unwrap();
        assert!(
            text.contains("machine apt.releases.hashicorp.com login ci password tok-very-private"),
            "{text}"
        );
        let meta = buckle_core::system::System::stat(
            &sys,
            std::path::Path::new("/etc/apt/auth.conf.d/hashicorp.conf"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(meta.mode, Some(0o600));
        let everything = format!(
            "{} {} {planned:?} {applied:?}",
            serde_json::to_string(&planned).unwrap(),
            serde_json::to_string(&applied).unwrap()
        );
        assert!(!everything.contains("tok-very-private"), "{everything}");
        assert_eq!(AptRepository.plan(&c, &s).await.unwrap(), Planned::NoOp);
        // Rotating the token is a change, still without the value.
        let rotated = crate::tasks::hostkit::testkit::cx_secrets(
            &sys,
            buckle_core::testing::MemorySource::new(),
            crate::tasks::hostkit::testkit::secrets("repo_token", "tok-rotated"),
        );
        let planned = AptRepository.plan(&rotated, &s).await.unwrap();
        assert!(planned.is_change());
        assert!(!serde_json::to_string(&planned).unwrap().contains("tok-"));
        // Removal deletes the credentials file with the source.
        AptRepository
            .remove(&c, &record_of("apt.repository", "hashicorp", &applied))
            .await
            .unwrap();
        assert!(!sys.exists("/etc/apt/auth.conf.d/hashicorp.conf"));
    }

    fn lookup_schema_marks_password() -> bool {
        serde_json::to_string(
            &buckle_core::task::lookup("apt.repository")
                .unwrap()
                .schema(),
        )
        .unwrap()
        .contains("x-buckle-secret")
    }
}
