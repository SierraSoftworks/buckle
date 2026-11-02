//! Secret providers turn a [`SecretRef`] into a value. The server resolves
//! secrets for agents; a local run resolves them itself.

use crate::model::SecretRef;
use async_trait::async_trait;

#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// Short name used in errors and logs: `env`, `file`, `exec`.
    fn kind(&self) -> &'static str;

    /// Whether this provider resolves references of this shape.
    fn handles(&self, reference: &SecretRef) -> bool;

    /// Resolves the reference. `Ok(None)` means "handled, but no such secret".
    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>>;
}

/// Reads secrets from the resolving process's environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvProvider;

#[async_trait]
impl SecretProvider for EnvProvider {
    fn kind(&self) -> &'static str {
        "env"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::Env { .. })
    }

    async fn resolve(&self, _name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::Env { env: var } = reference else {
            return Ok(None);
        };
        Ok(std::env::var(var).ok())
    }
}

/// Reads secrets from files (one value per file, a single trailing newline trimmed).
/// Typically fed by a Nomad `template` stanza rendering Nomad Variables.
#[derive(Debug, Clone, Default)]
pub struct FileProvider {
    /// When set, relative paths resolve against this root and absolute paths
    /// outside it are refused.
    pub root: Option<std::path::PathBuf>,
}

#[async_trait]
impl SecretProvider for FileProvider {
    fn kind(&self) -> &'static str {
        "file"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::File { .. })
    }

    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::File { file: path } = reference else {
            return Ok(None);
        };
        let path = match &self.root {
            Some(root) => {
                let joined = if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                };
                if !joined.starts_with(root) {
                    return Err(crate::error::user(
                        format!("The secret '{name}' points outside the configured secrets root."),
                        &["Keep secret files under the `secrets.providers.file.root` directory."],
                    ));
                }
                joined
            }
            None => path.clone(),
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(mut content) => {
                if content.ends_with('\n') {
                    content.pop();
                    if content.ends_with('\r') {
                        content.pop();
                    }
                }
                Ok(Some(content))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(crate::error::wrap_user(
                e,
                format!("The secret file for '{name}' could not be read."),
                &["Check the file's permissions and that the path in buckle.yaml is correct."],
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("value\n", "value")]
    #[case("value\r\n", "value")]
    #[case("value", "value")]
    #[case("two\nlines\n", "two\nlines")]
    #[tokio::test]
    async fn file_provider_trims_one_trailing_newline(
        #[case] content: &str,
        #[case] expected: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        std::fs::write(&path, content).unwrap();
        let got = FileProvider::default()
            .resolve("s", &SecretRef::File { file: path })
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn file_provider_refuses_paths_outside_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FileProvider {
            root: Some(dir.path().join("secrets")),
        };
        let err = provider
            .resolve(
                "x",
                &SecretRef::File {
                    file: "/etc/passwd".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[tokio::test]
    async fn file_provider_reports_missing_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let got = FileProvider::default()
            .resolve(
                "x",
                &SecretRef::File {
                    file: dir.path().join("nope"),
                },
            )
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn env_provider_reads_variables() {
        // SAFETY: tests in this module run single-threaded with respect to this variable.
        unsafe { std::env::set_var("BUCKLE_TEST_SECRET_ENV", "shh") };
        let got = EnvProvider
            .resolve(
                "x",
                &SecretRef::Env {
                    env: "BUCKLE_TEST_SECRET_ENV".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("shh"));
        assert!(!EnvProvider.handles(&SecretRef::File { file: "/x".into() }));
    }
}
