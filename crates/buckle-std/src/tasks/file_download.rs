//! `file.download`: fetch a URL to a file, verifying a checksum.

use super::hostkit::{self, describe_perms, digest_change};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::SecretField;
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FilePerms};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::instrument;

/// Downloads a URL to `dest`, optionally verifying a SHA-256 checksum.
///
/// ```yaml
/// - file.download:
///     url: https://releases.example.com/tool-1.2.3-linux-amd64
///     dest: /usr/local/bin/tool
///     mode: "0755"
///     checksum: "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
///     # or: checksum: https://releases.example.com/tool-1.2.3.sha256
///     headers: { Authorization: "Bearer {{ secrets.release_token }}" }   # values may use secrets
///     force: false
/// ```
///
/// With `checksum`, the file is up to date when its SHA-256 matches, and the
/// download is verified before anything is written: a mismatch is an error and
/// leaves `dest` untouched. The checksum may be `sha256:<hex>`, a bare hex
/// digest, or the URL of a `.sha256` / `SHA256SUMS` file (the line naming the
/// downloaded file is used). Without `checksum`, an existing `dest` is
/// considered up to date and is never re-downloaded, unless `force: true`,
/// which downloads on every apply (and so always shows as a change).
///
/// The download is held in memory and written atomically. Removal deletes
/// `dest` only if its content still matches what Buckle wrote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The URL to GET.
    pub url: String,
    /// Absolute destination path.
    pub dest: PathBuf,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// `sha256:<hex>`, a bare hex digest, or the URL of a file containing one.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Extra request headers (also sent when fetching a checksum URL). Values
    /// may reference secrets: `Authorization: "Bearer {{ secrets.token }}"`.
    #[serde(default)]
    pub headers: BTreeMap<String, SecretField>,
    /// Download again even if `dest` exists (when no checksum is given).
    #[serde(default)]
    pub force: bool,
}

pub struct FileDownload;
buckle_core::register_task!(FileDownload);

fn header_list(cx: &Cx, spec: &Spec) -> Result<Vec<(String, String)>> {
    hostkit::resolve_headers(cx, &spec.headers)
}

/// Resolves the expected SHA-256 (lowercase hex) from the `checksum` field.
pub(crate) async fn resolve_checksum(
    cx: &Cx,
    checksum: &str,
    url: &str,
    dest: &std::path::Path,
    headers: &[(String, String)],
) -> Result<String> {
    let checksum = checksum.trim();
    if let Some(hex) = checksum.strip_prefix("sha256:") {
        return if hostkit::is_sha256_hex(hex.trim()) {
            Ok(hex.trim().to_ascii_lowercase())
        } else {
            Err(error::user(
                format!("The checksum '{checksum}' is not a SHA-256 digest (64 hex characters)."),
                &["Use `sha256:<64 hex characters>` or the URL of a .sha256 file."],
            ))
        };
    }
    if hostkit::is_sha256_hex(checksum) {
        return Ok(checksum.to_ascii_lowercase());
    }
    if checksum.starts_with("http://") || checksum.starts_with("https://") {
        let body = hostkit::http_get(cx, checksum, headers).await?;
        let text = String::from_utf8_lossy(&body);
        let dest_name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let url_name = hostkit::url_basename(url);
        return hostkit::parse_sha256_text(&text, &[url_name.as_str(), dest_name.as_str()])
            .ok_or_else(|| {
                error::user(
                    format!("No SHA-256 digest for '{url_name}' was found at {checksum}."),
                    &["The checksum file should contain `<digest>  <filename>` lines or a single digest."],
                )
            });
    }
    Err(error::user(
        format!("The checksum '{checksum}' is not understood."),
        &["Use `sha256:<hex>`, a bare 64-character hex digest, or the URL of a .sha256 file."],
    ))
}

struct Inspection {
    planned: Planned,
    expected: Option<String>,
    /// Only permissions differ; the content is already right.
    perms_only: bool,
}

impl FileDownload {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        let dest = spec.dest.display();
        let headers = header_list(cx, spec)?;
        let expected = match &spec.checksum {
            Some(c) => Some(resolve_checksum(cx, c, &spec.url, &spec.dest, &headers).await?),
            None => None,
        };
        let Some(meta) = cx.system.stat(&spec.dest).await? else {
            return Ok(Inspection {
                planned: Planned::change(Diff::Create {
                    summary: format!("download {} to {dest}", spec.url),
                    preview: None,
                }),
                expected,
                perms_only: false,
            });
        };
        if meta.kind == FileKind::Directory {
            return Err(error::user(
                format!("{dest} is a directory, so file.download cannot write a file there."),
                &["Point `dest` at the file path, not its directory."],
            ));
        }
        let perms = hostkit::perm_changes(&meta, &spec.perms);
        let content_ok = match &expected {
            Some(want) => {
                let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
                let have = Digest::of(&current);
                if have.as_str() != want {
                    return Ok(Inspection {
                        planned: Planned::change(Diff::Update {
                            summary: format!(
                                "re-download {} to {dest}: {}",
                                spec.url,
                                digest_change(&have, &Digest::from_hex(want.clone()))
                            ),
                            unified: None,
                        }),
                        expected,
                        perms_only: false,
                    });
                }
                true
            }
            None if spec.force => {
                return Ok(Inspection {
                    planned: Planned::change(Diff::Update {
                        summary: format!("re-download {} to {dest} (force)", spec.url),
                        unified: None,
                    }),
                    expected,
                    perms_only: false,
                });
            }
            None => true,
        };
        let planned = if content_ok && perms.is_empty() {
            Planned::NoOp
        } else {
            Planned::change(Diff::Update {
                summary: format!("{dest}: {}", perms.join(", ")),
                unified: None,
            })
        };
        Ok(Inspection {
            planned,
            expected,
            perms_only: true,
        })
    }
}

#[async_trait]
impl Task for FileDownload {
    type Spec = Spec;
    const KIND: &'static str = "file.download";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Download a URL to a file, verifying a SHA-256 checksum, and write it atomically.";

    fn key(spec: &Spec) -> String {
        spec.dest.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        if perms.is_empty() {
            format!("{} from {}", spec.dest.display(), spec.url)
        } else {
            format!("{} ({perms}) from {}", spec.dest.display(), spec.url)
        }
    }

    #[instrument(name = "task.file.download.plan", skip_all, fields(url = %spec.url, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.file.download.apply", skip_all, fields(url = %spec.url, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let inspection = self.inspect(cx, spec).await?;
        if !inspection.planned.is_change() {
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
        if inspection.perms_only {
            cx.system.set_perms(&spec.dest, &spec.perms).await?;
            let current = cx.system.read(&spec.dest).await?.unwrap_or_default();
            // A file Buckle did not download is adopted, not claimed.
            return Ok(hostkit::ownership::file_written(
                cx,
                format!("updated permissions of {}", spec.dest.display()),
                &spec.dest,
                false,
                Digest::of(&current),
                None,
                true,
            ));
        }
        let body = hostkit::http_get(cx, &spec.url, &header_list(cx, spec)?).await?;
        let digest = Digest::of(&body);
        if let Some(want) = &inspection.expected
            && digest.as_str() != want
        {
            return Err(error::user(
                format!(
                    "The download of {} does not match its checksum: expected {want}, got {digest}. Nothing was written to {}.",
                    spec.url,
                    spec.dest.display()
                ),
                &[
                    "Check that the `checksum` belongs to this exact URL and version.",
                    "If the file changed upstream, update the checksum after verifying the new file.",
                ],
            ));
        }
        let existing = cx.system.stat(&spec.dest).await?;
        let perms = hostkit::effective_perms(&spec.perms, existing.as_ref());
        cx.system.write_atomic(&spec.dest, &body, &perms).await?;
        Ok(hostkit::written(
            format!("downloaded {} ({} bytes)", spec.url, body.len()),
            &spec.dest,
            digest,
            None,
            true,
        ))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "delete {} if it is unchanged since Buckle downloaded it",
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
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    const URL: &str = "https://dl.example.com/tool-1.0";
    const BODY: &str = "tool binary v1";

    fn sha(body: &str) -> String {
        Digest::of(body.as_bytes()).as_str().to_owned()
    }

    fn host() -> FakeSystem {
        FakeSystem::new().on_http_ok(URL, BODY)
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"url": URL, "dest": "/usr/local/bin/tool"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[rstest]
    #[case::missing(None, json!({}), Class::Change)]
    #[case::exists_without_checksum(Some("anything"), json!({}), Class::NoOp)]
    #[case::exists_but_forced(Some("anything"), json!({"force": true}), Class::Change)]
    #[case::checksum_matches(Some(BODY), json!({"checksum": format!("sha256:{}", sha(BODY))}), Class::NoOp)]
    #[case::checksum_differs(Some("stale"), json!({"checksum": format!("sha256:{}", sha(BODY))}), Class::Change)]
    #[case::forced_but_checksum_matches(Some(BODY), json!({"force": true, "checksum": sha(BODY)}), Class::NoOp)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] current: Option<&str>,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        let mut sys = host();
        if let Some(c) = current {
            sys = sys.with_file("/usr/local/bin/tool", c, FilePerms::mode(0o755));
        }
        assert_eq!(
            plan_class(&FileDownload, &cx(&sys), &s(extra)).await,
            expected
        );
    }

    #[tokio::test]
    async fn apply_downloads_verifies_and_is_idempotent() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(json!({"mode": "0755", "checksum": format!("sha256:{}", sha(BODY))}));
        let applied = apply(&FileDownload, &c, &spec).await;
        assert!(applied.changed);
        assert_eq!(sys.file_string("/usr/local/bin/tool").unwrap(), BODY);
        assert_eq!(applied.content_digest.as_ref().unwrap().as_str(), sha(BODY));
        assert_eq!(plan_class(&FileDownload, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn a_checksum_mismatch_writes_nothing() {
        let sys = host();
        let c = cx(&sys);
        let bad = format!("sha256:{}", sha("something else"));
        let err = FileDownload
            .apply(&c, &s(json!({"checksum": bad})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(
            err.description().contains("does not match its checksum"),
            "{}",
            err.description()
        );
        assert!(!sys.exists("/usr/local/bin/tool"), "no file is left behind");
        assert!(
            sys.paths()
                .iter()
                .all(|p| p.parent() != Some(std::path::Path::new("/usr/local/bin")))
        );

        // The same holds when a stale file already exists: it is not replaced.
        let sys = host().with_file("/usr/local/bin/tool", "old", FilePerms::default());
        let err = FileDownload
            .apply(&cx(&sys), &s(json!({"checksum": bad})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        assert_eq!(sys.file_string("/usr/local/bin/tool").unwrap(), "old");
    }

    #[rstest]
    #[case::single_digest(format!("{}\n", sha(BODY)))]
    #[case::digest_and_name(format!("{}  tool-1.0\n", sha(BODY)))]
    #[case::sums_file(format!("{}  other\n{}  tool-1.0\n", sha("zzz"), sha(BODY)))]
    #[tokio::test]
    async fn checksums_can_be_fetched_from_a_url(#[case] sums: String) {
        let sys = host().on_http_ok("https://dl.example.com/tool-1.0.sha256", &sums);
        let c = cx(&sys);
        let spec = s(json!({"checksum": "https://dl.example.com/tool-1.0.sha256"}));
        apply(&FileDownload, &c, &spec).await;
        assert_eq!(sys.file_string("/usr/local/bin/tool").unwrap(), BODY);
        assert_eq!(plan_class(&FileDownload, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn unusable_checksums_and_http_errors_are_user_errors() {
        let sys = host().on_http("https://dl.example.com/missing", |_| {
            buckle_core::system::HttpResponse {
                status: 404,
                headers: vec![],
                body: vec![],
            }
        });
        let c = cx(&sys);
        for bad in ["md5:abc", "sha256:xyz", "garbage"] {
            let err = FileDownload
                .plan(&c, &s(json!({"checksum": bad})))
                .await
                .unwrap_err();
            assert!(err.is(buckle_core::error::Kind::User), "{bad}");
        }
        let spec: Spec = spec(json!({"url": "https://dl.example.com/missing", "dest": "/x"}));
        let err = FileDownload
            .apply(&c, &spec, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("HTTP 404"));
    }

    #[tokio::test]
    async fn headers_are_sent_with_the_request() {
        let sys = host();
        let c = cx(&sys);
        apply(
            &FileDownload,
            &c,
            &s(json!({"headers": {"Authorization": "Bearer t"}})),
        )
        .await;
        let req = &sys.http_requests()[0];
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer t")
        );
    }

    #[tokio::test]
    async fn perms_only_drift_does_not_redownload() {
        let sys = host().with_file("/usr/local/bin/tool", BODY, FilePerms::mode(0o644));
        let c = cx(&sys);
        let spec = s(json!({"mode": "0755", "checksum": sha(BODY)}));
        apply(&FileDownload, &c, &spec).await;
        assert!(
            sys.http_requests().iter().all(|r| r.url != URL),
            "the binary was not fetched again"
        );
        assert_eq!(plan_class(&FileDownload, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn removal_respects_drift() {
        let sys = host();
        let c = cx(&sys);
        let rec = record(
            "file.download",
            "k",
            &apply(&FileDownload, &c, &s(json!({}))).await,
        );
        put(&sys, "/usr/local/bin/tool", "patched").await;
        assert!(!FileDownload.remove(&c, &rec).await.unwrap().changed);
        assert!(sys.exists("/usr/local/bin/tool"));
        put(&sys, "/usr/local/bin/tool", BODY).await;
        assert!(FileDownload.remove(&c, &rec).await.unwrap().changed);
    }

    #[test]
    fn sums_parsing_prefers_the_named_line() {
        let text = format!("{}  a\n{}  *b\n", sha("1"), sha("2"));
        assert_eq!(hostkit::parse_sha256_text(&text, &["b"]), Some(sha("2")));
        assert_eq!(hostkit::parse_sha256_text(&text, &["c"]), None);
    }
}
