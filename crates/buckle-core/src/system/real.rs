//! The real host. Lane 1 (core engine) owns the completion of this file; the
//! skeleton implements the filesystem, process and lookup operations directly
//! with tokio and std, and leaves ownership changes to `chown` where nix is
//! not available.

use super::*;
use crate::error::HumanizableResult;

/// The production [`System`].
#[derive(Debug, Default, Clone)]
pub struct RealSystem {
    http: Option<std::sync::Arc<dyn HttpClient>>,
}

/// HTTP is injected so that the core crate does not depend on an HTTP client;
/// the binary wires in reqwest.
#[async_trait]
pub trait HttpClient: Send + Sync + std::fmt::Debug {
    async fn send(&self, request: HttpRequest) -> crate::Result<HttpResponse>;
}

impl RealSystem {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_http(mut self, http: std::sync::Arc<dyn HttpClient>) -> Self {
        self.http = Some(http);
        self
    }

    #[cfg(unix)]
    fn meta_from(md: &std::fs::Metadata) -> FileMeta {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let kind = if md.file_type().is_symlink() {
            FileKind::Symlink
        } else if md.is_dir() {
            FileKind::Directory
        } else if md.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        };
        FileMeta {
            kind,
            size: md.len(),
            mode: Some(md.permissions().mode() & 0o7777),
            uid: Some(md.uid()),
            gid: Some(md.gid()),
            owner: nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(md.uid()))
                .ok()
                .flatten()
                .map(|u| u.name),
            group: nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(md.gid()))
                .ok()
                .flatten()
                .map(|g| g.name),
            modified: md
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from),
        }
    }

    #[cfg(not(unix))]
    fn meta_from(md: &std::fs::Metadata) -> FileMeta {
        let kind = if md.file_type().is_symlink() {
            FileKind::Symlink
        } else if md.is_dir() {
            FileKind::Directory
        } else if md.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        };
        FileMeta {
            kind,
            size: md.len(),
            mode: None,
            uid: None,
            gid: None,
            owner: None,
            group: None,
            modified: md
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from),
        }
    }

    async fn apply_perms(path: &Path, perms: &FilePerms) -> crate::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = perms.mode {
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .await
                    .to_human_error()?;
            }
            if perms.owner.is_some() || perms.group.is_some() {
                let uid = match &perms.owner {
                    Some(name) => Some(
                        nix::unistd::User::from_name(name)
                            .ok()
                            .flatten()
                            .ok_or_else(|| {
                                crate::error::user(
                                    format!("The user '{name}' does not exist on this host."),
                                    &["Create the user first (for example with a `user.account` task that runs before this one)."],
                                )
                            })?
                            .uid,
                    ),
                    None => None,
                };
                let gid = match &perms.group {
                    Some(name) => Some(
                        nix::unistd::Group::from_name(name)
                            .ok()
                            .flatten()
                            .ok_or_else(|| {
                                crate::error::user(
                                    format!("The group '{name}' does not exist on this host."),
                                    &["Create the group first (for example with a `user.group` task that runs before this one)."],
                                )
                            })?
                            .gid,
                    ),
                    None => None,
                };
                nix::unistd::chown(path, uid, gid).map_err(|e| {
                    crate::error::wrap_user(
                        e,
                        format!("Could not change the ownership of {}.", path.display()),
                        &["Buckle must run as root (or own the file) to change ownership."],
                    )
                })?;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (path, perms);
        }
        Ok(())
    }
}

#[async_trait]
impl System for RealSystem {
    fn platform(&self) -> Platform {
        Platform::current()
    }

    fn hostname(&self) -> String {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|s| s.trim().to_owned())
            })
            .unwrap_or_else(|| "localhost".to_owned())
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    async fn read(&self, path: &Path) -> crate::Result<Option<Vec<u8>>> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).to_human_error(),
        }
    }

    async fn stat(&self, path: &Path) -> crate::Result<Option<FileMeta>> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(md) => Ok(Some(Self::meta_from(&md))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).to_human_error(),
        }
    }

    async fn write_atomic(
        &self,
        path: &Path,
        bytes: &[u8],
        perms: &FilePerms,
    ) -> crate::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            crate::error::user(
                format!(
                    "Cannot write to {}: it has no parent directory.",
                    path.display()
                ),
                &["Use an absolute path to a file."],
            )
        })?;
        tokio::fs::create_dir_all(parent).await.to_human_error()?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = parent.join(format!(".{file_name}.buckle-tmp-{}", std::process::id()));
        tokio::fs::write(&tmp, bytes).await.to_human_error()?;
        if let Err(e) = Self::apply_perms(&tmp, perms).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        tokio::fs::rename(&tmp, path).await.to_human_error()?;
        Ok(())
    }

    async fn set_perms(&self, path: &Path, perms: &FilePerms) -> crate::Result<()> {
        Self::apply_perms(path, perms).await
    }

    async fn mkdir_all(&self, path: &Path, perms: &FilePerms) -> crate::Result<()> {
        tokio::fs::create_dir_all(path).await.to_human_error()?;
        Self::apply_perms(path, perms).await
    }

    async fn remove(&self, path: &Path, recursive: bool) -> crate::Result<bool> {
        let Some(meta) = self.stat(path).await? else {
            return Ok(false);
        };
        let result = match meta.kind {
            FileKind::Directory if recursive => tokio::fs::remove_dir_all(path).await,
            FileKind::Directory => tokio::fs::remove_dir(path).await,
            _ => tokio::fs::remove_file(path).await,
        };
        match result {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).to_human_error(),
        }
    }

    async fn symlink(&self, target: &Path, link: &Path) -> crate::Result<()> {
        #[cfg(unix)]
        {
            tokio::fs::symlink(target, link).await.to_human_error()
        }
        #[cfg(windows)]
        {
            tokio::fs::symlink_file(target, link).await.to_human_error()
        }
    }

    async fn read_link(&self, path: &Path) -> crate::Result<Option<PathBuf>> {
        match tokio::fs::read_link(path).await {
            Ok(p) => Ok(Some(p)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).to_human_error(),
        }
    }

    async fn list_dir(&self, path: &Path) -> crate::Result<Option<Vec<PathBuf>>> {
        let mut rd = match tokio::fs::read_dir(path).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).to_human_error(),
        };
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.to_human_error()? {
            out.push(entry.path());
        }
        out.sort();
        Ok(Some(out))
    }

    async fn rename(&self, from: &Path, to: &Path) -> crate::Result<()> {
        tokio::fs::rename(from, to).await.to_human_error()
    }

    async fn run(&self, command: Command) -> crate::Result<Output> {
        use tokio::io::AsyncWriteExt;
        let mut cmd = tokio::process::Command::new(&command.program);
        cmd.args(&command.args);
        if command.clear_env {
            cmd.env_clear();
        }
        cmd.envs(command.env.iter());
        if let Some(cwd) = &command.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(if command.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            crate::error::wrap_user(
                e,
                format!("Failed to start '{}'.", command.program),
                &["Make sure the program is installed and on the PATH of the Buckle process."],
            )
        })?;
        if let Some(stdin) = &command.stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            let _ = pipe.write_all(stdin).await;
            drop(pipe);
        }
        let wait = child.wait_with_output();
        let (result, timed_out) = match command.timeout {
            Some(t) => match tokio::time::timeout(t, wait).await {
                Ok(r) => (r, false),
                Err(_) => {
                    return Ok(Output {
                        code: None,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        timed_out: true,
                    });
                }
            },
            None => (wait.await, false),
        };
        let out = result.to_human_error()?;
        Ok(Output {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
            timed_out,
        })
    }

    async fn which(&self, program: &str) -> crate::Result<Option<PathBuf>> {
        if program.contains(std::path::MAIN_SEPARATOR) {
            let p = PathBuf::from(program);
            return Ok(if p.exists() { Some(p) } else { None });
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    async fn lookup_user(&self, name: &str) -> crate::Result<Option<UserInfo>> {
        #[cfg(unix)]
        {
            let user = nix::unistd::User::from_name(name).map_err(|e| {
                crate::error::wrap_system(
                    e,
                    "Looking up a user account failed.",
                    &["Check that the system's user database is readable."],
                )
            })?;
            Ok(user.map(|u| UserInfo {
                name: u.name,
                uid: u.uid.as_raw(),
                gid: u.gid.as_raw(),
                home: u.dir,
                shell: u.shell,
                gecos: u.gecos.to_string_lossy().into_owned(),
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Ok(None)
        }
    }

    async fn lookup_group(&self, name: &str) -> crate::Result<Option<GroupInfo>> {
        #[cfg(unix)]
        {
            let group = nix::unistd::Group::from_name(name).map_err(|e| {
                crate::error::wrap_system(
                    e,
                    "Looking up a group failed.",
                    &["Check that the system's group database is readable."],
                )
            })?;
            Ok(group.map(|g| GroupInfo {
                name: g.name,
                gid: g.gid.as_raw(),
                members: g.mem,
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Ok(None)
        }
    }

    async fn list_users(&self) -> crate::Result<Vec<UserInfo>> {
        #[cfg(unix)]
        {
            let content = tokio::fs::read_to_string("/etc/passwd")
                .await
                .to_human_error()?;
            Ok(content
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .filter_map(|line| {
                    let f: Vec<&str> = line.split(':').collect();
                    if f.len() < 7 {
                        return None;
                    }
                    Some(UserInfo {
                        name: f[0].to_owned(),
                        uid: f[2].parse().ok()?,
                        gid: f[3].parse().ok()?,
                        gecos: f[4].to_owned(),
                        home: PathBuf::from(f[5]),
                        shell: PathBuf::from(f[6]),
                    })
                })
                .collect())
        }
        #[cfg(not(unix))]
        {
            Ok(Vec::new())
        }
    }

    async fn http(&self, request: HttpRequest) -> crate::Result<HttpResponse> {
        match &self.http {
            Some(client) => client.send(request).await,
            None => Err(crate::error::system(
                "No HTTP client is configured for this System.",
                &["This is a wiring bug: the binary must attach an HTTP client to RealSystem."],
            )),
        }
    }

    async fn tcp_connect(&self, address: &str, timeout: Duration) -> crate::Result<bool> {
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(_)) | Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_atomic_creates_parents_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new();
        let path = dir.path().join("a/b/c.txt");
        sys.write_atomic(&path, b"hello", &FilePerms::mode(0o600))
            .await
            .unwrap();
        assert_eq!(sys.read(&path).await.unwrap().unwrap(), b"hello");
        let meta = sys.stat(&path).await.unwrap().unwrap();
        assert_eq!(meta.kind, FileKind::File);
        #[cfg(unix)]
        assert_eq!(meta.mode, Some(0o600));
        assert!(
            sys.read(&dir.path().join("missing"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(sys.remove(&path, false).await.unwrap());
        assert!(!sys.remove(&path, false).await.unwrap());
    }

    #[tokio::test]
    async fn run_captures_output_and_exit_code() {
        let sys = RealSystem::new();
        let out = sys
            .run(
                Command::new("sh")
                    .arg("-c")
                    .arg("echo out; echo err >&2; exit 3"),
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout_str().trim(), "out");
        assert_eq!(out.stderr_str().trim(), "err");
        assert!(!out.success());
    }

    #[tokio::test]
    async fn run_honours_timeouts() {
        let sys = RealSystem::new();
        let out = sys
            .run(
                Command::new("sleep")
                    .arg("5")
                    .timeout(Duration::from_millis(100)),
            )
            .await
            .unwrap();
        assert!(out.timed_out);
    }

    #[tokio::test]
    async fn missing_programs_are_user_errors() {
        let sys = RealSystem::new();
        let err = sys
            .run(Command::new("definitely-not-a-program-buckle"))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(
            sys.which("definitely-not-a-program-buckle")
                .await
                .unwrap()
                .is_none()
        );
        assert!(sys.which("sh").await.unwrap().is_some());
    }
}
