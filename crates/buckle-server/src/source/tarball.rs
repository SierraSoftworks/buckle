//! Deterministic revision tarballs: sorted entries, zeroed times and owners,
//! so the same tree always produces the same digest.

use buckle_core::error::HumanizableResult;
use buckle_core::{Digest, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Every regular file under `root` (relative paths, `/`-separated, sorted),
/// skipping `.git`.
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        for entry in std::fs::read_dir(&dir).to_human_error()? {
            let entry = entry.to_human_error()?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = rel.join(&name);
            let kind = entry.file_type().to_human_error()?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Writes `root` as a gzipped tar to `dest` and returns its bytes' digest.
pub fn create(root: &Path, dest: &Path) -> Result<Digest> {
    let bytes = build(root)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).to_human_error()?;
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &bytes).to_human_error()?;
    std::fs::rename(&tmp, dest).to_human_error()?;
    Ok(Digest::of(&bytes))
}

/// The gzipped tar of `root`, in memory.
pub fn build(root: &Path) -> Result<Vec<u8>> {
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for rel in files(root)? {
        let full = root.join(&rel);
        let content = std::fs::read(&full).to_human_error()?;
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(file_mode(&full));
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        let name = rel.to_string_lossy().replace('\\', "/");
        tar.append_data(&mut header, &name, content.as_slice())
            .map_err(tar_error)?;
    }
    let gz = tar.into_inner().map_err(tar_error)?;
    let mut bytes = gz.finish().map_err(tar_error)?;
    bytes.flush().map_err(tar_error)?;
    Ok(bytes)
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o644);
    if mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

/// Extracts a tarball written by [`create`] into `dest`.
pub fn extract(tarball: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(tarball).to_human_error()?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(dest).to_human_error()?;
    archive.unpack(dest).map_err(tar_error)
}

fn tar_error(e: std::io::Error) -> buckle_core::Error {
    buckle_core::error::wrap_system(
        e,
        "A revision tarball could not be written or read.",
        &["Check the server's data directory for disk space and permissions."],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarballs_are_deterministic_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("modules/a/files")).unwrap();
        std::fs::write(dir.path().join("modules/a/module.yaml"), "name: a\n").unwrap();
        std::fs::write(dir.path().join("modules/a/files/x.j2"), "hi").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref").unwrap();
        let a = build(dir.path()).unwrap();
        let b = build(dir.path()).unwrap();
        assert_eq!(Digest::of(&a), Digest::of(&b));

        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("r.tar.gz");
        let digest = create(dir.path(), &path).unwrap();
        assert_eq!(digest, Digest::of(&a));
        let target = out.path().join("tree");
        extract(&path, &target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("modules/a/files/x.j2")).unwrap(),
            "hi"
        );
        assert!(!target.join(".git").exists());
    }
}
