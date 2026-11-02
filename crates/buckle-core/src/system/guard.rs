//! The plan-mode guard: a [`System`] that refuses to change the host.
//!
//! Planning is pure (design §04): it may read anything and run probes, but a
//! task that writes while planning is a bug that would change hosts during a
//! "dry" plan or a server preview. The engine wraps the host's system in a
//! [`PlanGuard`] for everything that runs in [`crate::task::Mode::Plan`]; a
//! mutation outside the scratch directories fails loudly instead.

use super::*;
use std::sync::Arc;

/// Delegates reads, probes and process runs to `inner`; refuses writes,
/// permission changes, removals, symlinks and renames outside `allowed`.
pub struct PlanGuard {
    inner: Arc<dyn System>,
    allowed: Vec<PathBuf>,
}

impl PlanGuard {
    /// Guards `inner`, allowing mutations only under its
    /// [`System::temp_dir`] and the `extra` directories (e.g.
    /// `<state_dir>/tmp`).
    pub fn new(inner: Arc<dyn System>, extra: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut allowed = vec![inner.temp_dir()];
        allowed.extend(extra);
        Self { inner, allowed }
    }

    fn check(&self, what: &str, path: &Path) -> crate::Result<()> {
        let inside = !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
            && self.allowed.iter().any(|root| path.starts_with(root));
        if inside {
            return Ok(());
        }
        Err(crate::error::system(
            format!(
                "Planning tried to {what} {}; a plan must not change the host.",
                path.display()
            ),
            &[
                "This is a bug in the task kind: its `plan` must only inspect the host. Move the change to `apply`.",
                "Scratch files belong in the system's temp directory.",
            ],
        ))
    }
}

#[async_trait]
impl System for PlanGuard {
    fn platform(&self) -> Platform {
        self.inner.platform()
    }

    fn hostname(&self) -> String {
        self.inner.hostname()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.env_var(name)
    }

    async fn read(&self, path: &Path) -> crate::Result<Option<Vec<u8>>> {
        self.inner.read(path).await
    }

    async fn stat(&self, path: &Path) -> crate::Result<Option<FileMeta>> {
        self.inner.stat(path).await
    }

    async fn write_atomic(
        &self,
        path: &Path,
        bytes: &[u8],
        perms: &FilePerms,
    ) -> crate::Result<()> {
        self.check("write", path)?;
        self.inner.write_atomic(path, bytes, perms).await
    }

    async fn set_perms(&self, path: &Path, perms: &FilePerms) -> crate::Result<()> {
        self.check("change the permissions of", path)?;
        self.inner.set_perms(path, perms).await
    }

    async fn mkdir_all(&self, path: &Path, perms: &FilePerms) -> crate::Result<()> {
        self.check("create", path)?;
        self.inner.mkdir_all(path, perms).await
    }

    async fn remove(&self, path: &Path, recursive: bool) -> crate::Result<bool> {
        self.check("remove", path)?;
        self.inner.remove(path, recursive).await
    }

    async fn symlink(&self, target: &Path, link: &Path) -> crate::Result<()> {
        self.check("create the symlink", link)?;
        self.inner.symlink(target, link).await
    }

    async fn read_link(&self, path: &Path) -> crate::Result<Option<PathBuf>> {
        self.inner.read_link(path).await
    }

    async fn list_dir(&self, path: &Path) -> crate::Result<Option<Vec<PathBuf>>> {
        self.inner.list_dir(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> crate::Result<()> {
        self.check("move", from)?;
        self.check("replace", to)?;
        self.inner.rename(from, to).await
    }

    /// Probes (`unless`, version checks) run in plan mode by design.
    async fn run(&self, command: Command) -> crate::Result<Output> {
        self.inner.run(command).await
    }

    async fn which(&self, program: &str) -> crate::Result<Option<PathBuf>> {
        self.inner.which(program).await
    }

    async fn lookup_user(&self, name: &str) -> crate::Result<Option<UserInfo>> {
        self.inner.lookup_user(name).await
    }

    async fn lookup_group(&self, name: &str) -> crate::Result<Option<GroupInfo>> {
        self.inner.lookup_group(name).await
    }

    async fn list_users(&self) -> crate::Result<Vec<UserInfo>> {
        self.inner.list_users().await
    }

    async fn http(&self, request: HttpRequest) -> crate::Result<HttpResponse> {
        self.inner.http(request).await
    }

    async fn tcp_connect(&self, address: &str, timeout: Duration) -> crate::Result<bool> {
        self.inner.tcp_connect(address, timeout).await
    }

    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.inner.now()
    }

    fn temp_dir(&self) -> PathBuf {
        self.inner.temp_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeSystem;

    #[tokio::test]
    async fn reads_pass_and_writes_outside_scratch_are_refused() {
        let fake = FakeSystem::new().with_file("/etc/a", "x", FilePerms::default());
        let guard = PlanGuard::new(
            Arc::new(fake.clone()),
            [PathBuf::from("/var/lib/buckle/tmp")],
        );
        assert_eq!(
            guard.read(Path::new("/etc/a")).await.unwrap().unwrap(),
            b"x"
        );
        let err = guard
            .write_atomic(Path::new("/etc/a"), b"y", &FilePerms::default())
            .await
            .unwrap_err();
        assert!(
            err.message().contains("must not change the host"),
            "{}",
            err.message()
        );
        assert_eq!(fake.file_string("/etc/a").as_deref(), Some("x"));
        assert!(guard.remove(Path::new("/etc/a"), false).await.is_err());
        assert!(
            guard
                .set_perms(Path::new("/etc/a"), &FilePerms::mode(0o600))
                .await
                .is_err()
        );
        assert!(
            guard
                .mkdir_all(Path::new("/opt/x"), &FilePerms::default())
                .await
                .is_err()
        );
        assert!(
            guard
                .symlink(Path::new("/etc/a"), Path::new("/etc/b"))
                .await
                .is_err()
        );
        assert!(
            guard
                .rename(Path::new("/etc/a"), Path::new("/tmp/a"))
                .await
                .is_err()
        );
        let scratch = guard.temp_dir().join("validate/conf");
        guard
            .write_atomic(&scratch, b"ok", &FilePerms::default())
            .await
            .unwrap();
        guard
            .write_atomic(
                Path::new("/var/lib/buckle/tmp/x"),
                b"ok",
                &FilePerms::default(),
            )
            .await
            .unwrap();
        assert!(
            guard
                .write_atomic(
                    &guard.temp_dir().join("../etc/escape"),
                    b"no",
                    &FilePerms::default()
                )
                .await
                .is_err()
        );
    }
}
