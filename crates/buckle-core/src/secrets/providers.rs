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

/// Runs a program and uses its stdout as the value: `{ exec: [op, read, "op://vault/item"] }`.
///
/// **Development only.** Design §06: production secrets arrive through the
/// `file` and `env` providers (Nomad renders them); `exec` exists so that a
/// developer can point a local run at a password manager CLI. It is off unless
/// the server or agent configuration lists the program in an allowlist, and an
/// empty allowlist (the default) refuses everything.
///
/// * The program (`exec[0]`) must equal an allowlist entry exactly. A bare
///   name such as `op` is looked up on `PATH`; an absolute path pins one
///   binary. `op` does not permit `/tmp/op`.
/// * The value is stdout with one trailing newline (`\n` or `\r\n`) trimmed.
/// * A non-zero exit is a user error that includes stderr but never stdout,
///   because stdout may be (part of) the secret.
/// * The child runs with stdin closed and is killed after the timeout
///   (30 seconds by default).
///
/// ```
/// use buckle_core::model::SecretRef;
/// use buckle_core::secrets::providers::{ExecProvider, SecretProvider};
///
/// let provider = ExecProvider::new(vec!["op".into()]);
/// assert!(provider.allows("op"));
/// assert!(!provider.allows("/tmp/op"));
/// assert!(provider.handles(&SecretRef::Exec { exec: vec!["op".into()] }));
/// ```
#[derive(Debug, Clone)]
pub struct ExecProvider {
    allow: Vec<String>,
    timeout: std::time::Duration,
}

impl Default for ExecProvider {
    /// An exec provider with an empty allowlist: every program is refused.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ExecProvider {
    /// The default time a secret program may run before it is killed.
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// A provider permitted to run exactly the programs in `allow`.
    pub fn new(allow: Vec<String>) -> Self {
        Self {
            allow,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Overrides how long a program may run.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether `program` (an `exec[0]`) is on the allowlist.
    pub fn allows(&self, program: &str) -> bool {
        self.allow.iter().any(|a| a == program)
    }

    /// The configured allowlist.
    pub fn allowlist(&self) -> &[String] {
        &self.allow
    }
}

/// Most of a program's stderr that is quoted in an error.
const STDERR_LIMIT: usize = 4096;

fn quote_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "(no output on stderr)".to_owned();
    }
    if text.len() <= STDERR_LIMIT {
        return text.to_owned();
    }
    let mut cut = STDERR_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… (truncated)", &text[..cut])
}

#[async_trait]
impl SecretProvider for ExecProvider {
    fn kind(&self) -> &'static str {
        "exec"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::Exec { .. })
    }

    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::Exec { exec } = reference else {
            return Ok(None);
        };
        let Some((program, args)) = exec.split_first() else {
            return Err(crate::error::user(
                format!("The secret '{name}' has an empty `exec` command."),
                &[
                    "Give the program and its arguments, e.g. `{ exec: [op, read, \"op://vault/item/field\"] }`.",
                ],
            ));
        };
        if !self.allows(program) {
            return Err(crate::error::user(
                format!(
                    "The secret '{name}' runs '{program}', which is not on the exec provider's allowlist."
                ),
                &[
                    "Add the program to `secrets.providers.exec.allow` in the server or agent configuration (development only).",
                    "In production, deliver secrets with the file or env providers instead.",
                ],
            ));
        }

        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| {
            crate::error::wrap_user(
                e,
                format!("The program '{program}' for the secret '{name}' could not be started."),
                &["Check that the program is installed and on the PATH of the resolving process."],
            )
        })?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(crate::error::wrap_system(
                    e,
                    format!("Waiting for '{program}' to produce the secret '{name}' failed."),
                    &[
                        "Read the internal error message for details and report the issue if it persists.",
                    ],
                ));
            }
            Err(_) => {
                return Err(crate::error::user(
                    format!(
                        "The program '{program}' for the secret '{name}' did not finish within {}s.",
                        self.timeout.as_secs()
                    ),
                    &[
                        "Check whether the program is waiting for interactive input (for example a password manager that needs unlocking).",
                    ],
                ));
            }
        };
        if !output.status.success() {
            let status = match output.status.code() {
                Some(code) => format!("exit code {code}"),
                None => "a signal".to_owned(),
            };
            // Never include stdout: it may hold the secret, or part of it.
            return Err(crate::error::user(
                format!(
                    "The program '{program}' for the secret '{name}' failed with {status}: {}",
                    quote_stderr(&output.stderr)
                ),
                &[
                    "Run the command by hand to check that it prints the secret, e.g. that the password manager is signed in.",
                ],
            ));
        }
        let mut value = String::from_utf8(output.stdout).map_err(|_| {
            crate::error::user(
                format!("The program '{program}' printed a value for the secret '{name}' that is not valid UTF-8."),
                &["Secrets must be text; encode binary values (for example as base64) before printing them."],
            )
        })?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        Ok(Some(value))
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

    fn exec(argv: &[&str]) -> SecretRef {
        SecretRef::Exec {
            exec: argv.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[cfg(unix)]
    #[rstest]
    #[case("printf 'tok3n\\n'", "tok3n")]
    #[case("printf 'tok3n\\r\\n'", "tok3n")]
    #[case("printf 'tok3n'", "tok3n")]
    #[case("printf 'a\\nb\\n\\n'", "a\nb\n")]
    #[tokio::test]
    async fn exec_provider_runs_allowlisted_programs(#[case] script: &str, #[case] expected: &str) {
        let got = ExecProvider::new(vec!["sh".into()])
            .resolve("x", &exec(&["sh", "-c", script]))
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn exec_provider_refuses_programs_off_the_allowlist() {
        let provider = ExecProvider::new(vec!["op".into()]);
        for argv in [&["sh", "-c", "echo hi"][..], &["/tmp/op", "read"][..]] {
            let err = provider.resolve("db", &exec(argv)).await.unwrap_err();
            assert!(err.is(crate::error::Kind::User));
            assert!(err.message().contains(argv[0]), "{}", err.message());
            assert!(err.message().contains("'db'"));
        }
        let err = ExecProvider::default()
            .resolve("db", &exec(&["sh"]))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[tokio::test]
    async fn exec_provider_rejects_an_empty_command() {
        let err = ExecProvider::new(vec!["sh".into()])
            .resolve("db", &exec(&[]))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_provider_failure_quotes_stderr_but_never_stdout() {
        let err = ExecProvider::new(vec!["sh".into()])
            .resolve(
                "db",
                &exec(&[
                    "sh",
                    "-c",
                    "echo partial-SECRET-stdout; echo 'vault is locked' >&2; exit 3",
                ]),
            )
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        let shown = format!("{} {} {err:?}", err.message(), err.description());
        assert!(shown.contains("vault is locked"), "{shown}");
        assert!(shown.contains("exit code 3"), "{shown}");
        assert!(!shown.contains("partial-SECRET-stdout"), "{shown}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_provider_reports_missing_programs_and_timeouts() {
        let missing = ExecProvider::new(vec!["buckle-no-such-program".into()])
            .resolve("db", &exec(&["buckle-no-such-program"]))
            .await
            .unwrap_err();
        assert!(missing.is(crate::error::Kind::User));
        assert!(missing.message().contains("buckle-no-such-program"));

        let slow = ExecProvider::new(vec!["sleep".into()])
            .with_timeout(std::time::Duration::from_millis(100))
            .resolve("db", &exec(&["sleep", "5"]))
            .await
            .unwrap_err();
        assert!(slow.is(crate::error::Kind::User));
        assert!(slow.message().contains("did not finish"));
    }

    #[test]
    fn stderr_quotes_are_bounded() {
        let long = "é".repeat(STDERR_LIMIT);
        let quoted = quote_stderr(long.as_bytes());
        assert!(quoted.len() < STDERR_LIMIT + 32);
        assert!(quoted.ends_with("(truncated)"));
        assert_eq!(quote_stderr(b"  \n"), "(no output on stderr)");
    }
}
