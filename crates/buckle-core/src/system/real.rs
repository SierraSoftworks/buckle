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
    /// Reported instead of the running binary's platform (dry runs over facts).
    platform: Option<Platform>,
    /// Buckle's own directory on this host, when the caller knows it.
    state_dir: Option<PathBuf>,
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

    /// Reports `platform` from [`System::platform`] instead of the binary's own,
    /// so a dry run over another host's facts checks kinds against that host.
    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform);
        self
    }

    /// Reports `dir` from [`System::state_dir`] instead of the environment's
    /// default, so tasks keep their ledgers and scratch files where the
    /// agent or command was told to (`state_dir` in agent.yaml, `--state-dir`).
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
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

fn kernel_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty())
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Whether `path` is a file this process could execute: on Unix a regular
/// file with an execute bit; elsewhere any file (PATHEXT decides).
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Creates (or truncates) `path` with mode 0600, writes `bytes` and fsyncs.
async fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

/// The mode a new file gets when the caller does not ask for one: 0666 less
/// the process umask (read from /proc on Linux, without changing it), as if
/// the file had been created directly.
fn default_file_mode() -> Option<u32> {
    #[cfg(unix)]
    {
        let umask = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("Umask:"))
                    .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
            })
            .unwrap_or(0o022);
        Some(0o666 & !umask)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[async_trait]
impl System for RealSystem {
    fn platform(&self) -> Platform {
        self.platform.unwrap_or_else(Platform::current)
    }

    /// The kernel's hostname (`gethostname`), then `$HOSTNAME`, then
    /// `/etc/hostname`. A shell exports `HOSTNAME` but a service manager does
    /// not, and `/etc/hostname` can disagree with the running kernel, so the
    /// kernel wins. On Windows the computer name (`COMPUTERNAME`) comes first.
    fn hostname(&self) -> String {
        kernel_hostname()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|h| !h.trim().is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|h| !h.is_empty())
            })
            .unwrap_or_else(|| "localhost".to_owned())
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn state_dir(&self) -> PathBuf {
        match &self.state_dir {
            Some(dir) => dir.clone(),
            None => self
                .env_var("BUCKLE_STATE_DIR")
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/buckle")),
        }
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
        let prefix = super::temp_prefix_for(&file_name);
        super::remove_temp_leftovers(parent, &prefix);
        let tmp = parent.join(format!("{prefix}{}", std::process::id()));
        // The temp file starts private (0600) and only gets its final mode
        // and owner once the content is written, so a secret is never
        // readable through a wider mode, even briefly.
        let written = async {
            write_private(&tmp, bytes).await.to_human_error()?;
            let mut final_perms = perms.clone();
            if final_perms.mode.is_none() {
                final_perms.mode = default_file_mode();
            }
            Self::apply_perms(&tmp, &final_perms).await?;
            tokio::fs::rename(&tmp, path).await.to_human_error()?;
            crate::state::sync_dir(Some(parent)).to_human_error()
        }
        .await;
        if written.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        written
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
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                Err(super::not_empty(path))
            }
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
        // Its own process group, so a timeout can kill everything it started
        // (a shell's children included), not just the direct child.
        #[cfg(unix)]
        cmd.process_group(0);
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
        let pid = child.id();
        let wait = child.wait_with_output();
        let (result, timed_out) = match command.timeout {
            Some(t) => match tokio::time::timeout(t, wait).await {
                Ok(r) => (r, false),
                Err(_) => {
                    #[cfg(unix)]
                    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                        let _ = nix::sys::signal::killpg(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                    #[cfg(not(unix))]
                    let _ = pid;
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
        if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
            let p = PathBuf::from(program);
            return Ok(is_executable(&p).then_some(p));
        }
        // On Windows an executable is found by name plus one of the PATHEXT
        // extensions (`sh` resolves to `sh.exe`); elsewhere the bare name is the file.
        let extensions: Vec<String> = if cfg!(windows) {
            let mut exts = vec![String::new()];
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
            exts.extend(
                pathext
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(|e| e.to_ascii_lowercase()),
            );
            exts
        } else {
            vec![String::new()]
        };
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            for ext in &extensions {
                let candidate = dir.join(format!("{program}{ext}"));
                if is_executable(&candidate) {
                    return Ok(Some(candidate));
                }
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

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_sets_perms_rather_than_merging_them() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new();
        let path = dir.path().join("conf");
        sys.write_atomic(&path, b"one", &FilePerms::mode(0o640))
            .await
            .unwrap();
        assert_eq!(mode_of(&path), 0o640);
        sys.write_atomic(&path, b"two", &FilePerms::default())
            .await
            .unwrap();
        assert_eq!(
            Some(mode_of(&path)),
            default_file_mode(),
            "no mode means a new file's default, not the replaced file's mode"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temp file is left behind");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_leaves_nothing_behind_when_perms_fail_and_clears_crash_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new();
        let path = dir.path().join("secret.conf");
        let leftover = dir.path().join(".secret.conf.buckle-tmp-999999");
        std::fs::write(&leftover, b"half").unwrap();
        let mut perms = FilePerms::mode(0o600);
        perms.owner = Some("no-such-user-buckle".into());
        assert!(sys.write_atomic(&path, b"s3cret", &perms).await.is_err());
        assert!(!path.exists());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[tokio::test]
    async fn removing_a_non_empty_directory_is_the_same_user_error_as_the_fake() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new();
        let sub = dir.path().join("d");
        sys.write_atomic(&sub.join("f"), b"x", &FilePerms::default())
            .await
            .unwrap();
        let real = sys.remove(&sub, false).await.unwrap_err();
        let fake_sys =
            crate::testing::FakeSystem::new().with_file(sub.join("f"), "x", FilePerms::default());
        let fake = fake_sys.remove(&sub, false).await.unwrap_err();
        assert!(real.is(crate::error::Kind::User));
        assert_eq!(real.message(), fake.message());
        assert!(sys.remove(&sub, true).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_follows_symlinks_like_the_fake() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new();
        let target = dir.path().join("target");
        sys.write_atomic(&target, b"through", &FilePerms::default())
            .await
            .unwrap();
        let link = dir.path().join("link");
        sys.symlink(Path::new("target"), &link).await.unwrap();
        let fake =
            crate::testing::FakeSystem::new().with_file(&target, "through", FilePerms::default());
        fake.symlink(Path::new("target"), &link).await.unwrap();
        assert_eq!(
            sys.read(&link).await.unwrap(),
            fake.read(&link).await.unwrap()
        );
        assert_eq!(fake.read(&link).await.unwrap().unwrap(), b"through");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let sys = RealSystem::new();
        let out = sys
            .run(
                Command::new("sh")
                    .arg("-c")
                    .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()))
                    .timeout(Duration::from_millis(300)),
            )
            .await
            .unwrap();
        assert!(out.timed_out);
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..100 {
            // Reaped by init once killed; ESRCH means gone.
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
                || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .map(|s| s.contains(") Z "))
                    .unwrap_or(true)
            {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "the shell's background child was killed too");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn which_requires_the_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        let sys = RealSystem::new();
        assert_eq!(sys.which(tool.to_str().unwrap()).await.unwrap(), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sys.which(tool.to_str().unwrap()).await.unwrap(), Some(tool));
    }

    #[cfg(unix)]
    #[test]
    fn hostname_prefers_the_kernel() {
        let kernel = nix::unistd::gethostname().unwrap().into_string().unwrap();
        assert_eq!(RealSystem::new().hostname(), kernel.trim());
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
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            sys.which(present).await.unwrap().is_some(),
            "{present} should be on the PATH"
        );
    }
}
