//! Revision bundles: the gzip tarball of `buckle.yaml`, `modules/` and
//! `vars/` the server streams through `FetchRevision`, and the content digest
//! local mode uses as a synthetic revision id.

use buckle_core::error::HumanizableResult;
use sha2::{Digest as _, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The largest revision bundle the agent accepts, compressed or not.
pub const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn normalise_digest(digest: &str) -> String {
    digest
        .trim()
        .trim_start_matches("sha256:")
        .to_ascii_lowercase()
}

/// Checks that `bytes` hash to `expected` (`<hex>` or `sha256:<hex>`).
pub fn verify_digest(bytes: &[u8], expected: &str) -> buckle_core::Result<()> {
    let actual = sha256_hex(bytes);
    if actual == normalise_digest(expected) {
        Ok(())
    } else {
        Err(buckle_core::error::user(
            format!(
                "The revision bundle is corrupt: its digest is {actual} but the server announced {}.",
                normalise_digest(expected)
            ),
            &[
                "The download was damaged in transit or the server's cache is inconsistent; the agent will fetch it again.",
            ],
        ))
    }
}

/// Whether two digests name the same content.
pub fn digests_match(a: &str, b: &str) -> bool {
    normalise_digest(a) == normalise_digest(b)
}

fn visible_entries(dir: &Path) -> buckle_core::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).to_human_error()? {
            let entry = entry.to_human_error()?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type().to_human_error()?;
            if file_type.is_dir() {
                stack.push(path.clone());
                out.push(path);
            } else if file_type.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Digest of a directory tree's visible files (names starting with `.` are
/// ignored, so `.git` and editor swap files do not count): stable across
/// machines and independent of timestamps.
pub fn tree_digest(dir: &Path) -> buckle_core::Result<String> {
    let mut hasher = Sha256::new();
    for path in visible_entries(dir)? {
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(dir).unwrap_or(&path);
        let rel = rel.to_string_lossy().replace('\\', "/");
        let content = std::fs::read(&path).to_human_error()?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Packs a configuration directory as a deterministic gzip tarball.
pub fn pack_dir(dir: &Path) -> buckle_core::Result<Vec<u8>> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for path in visible_entries(dir)? {
        let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        if path.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder
                .append_data(&mut header, &rel, std::io::empty())
                .to_human_error()?;
        } else {
            let content = std::fs::read(&path).to_human_error()?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            builder
                .append_data(&mut header, &rel, content.as_slice())
                .to_human_error()?;
        }
    }
    let encoder = builder.into_inner().to_human_error()?;
    encoder.finish().to_human_error()
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn bad_bundle(message: String) -> buckle_core::Error {
    buckle_core::error::user(
        message,
        &[
            "The server sent a revision bundle Buckle will not unpack; check the server's revision store.",
        ],
    )
}

/// Extracts a gzip tarball into `dest` atomically: the tree is unpacked next
/// to `dest` and renamed into place, so a crash never leaves a half-written
/// revision that looks complete. Only regular files and directories with
/// relative paths are accepted.
pub fn extract_atomic(bytes: &[u8], dest: &Path) -> buckle_core::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        buckle_core::error::system(
            format!("The revision cache path {} has no parent.", dest.display()),
            &["This is a bug; please report it."],
        )
    })?;
    std::fs::create_dir_all(parent).to_human_error()?;
    let staging = parent.join(format!(
        ".{}.staging-{:08x}",
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        rand::random::<u32>()
    ));
    let result = unpack_into(bytes, &staging).and_then(|()| {
        if dest.exists() {
            std::fs::remove_dir_all(dest).to_human_error()?;
        }
        std::fs::rename(&staging, dest).to_human_error()
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn unpack_into(bytes: &[u8], staging: &Path) -> buckle_core::Result<()> {
    std::fs::create_dir_all(staging).to_human_error()?;
    let decoder = flate2::read::GzDecoder::new(bytes).take(MAX_BUNDLE_BYTES);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            "The revision bundle is not a valid tarball.",
            &["The server sent a damaged bundle; the agent will fetch it again."],
        )
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                "The revision bundle is not a valid tarball.",
                &["The server sent a damaged bundle; the agent will fetch it again."],
            )
        })?;
        let path = entry.path().to_human_error()?.into_owned();
        if !safe_relative(&path) {
            return Err(bad_bundle(format!(
                "The revision bundle contains the unsafe path '{}'.",
                path.display()
            )));
        }
        let target = staging.join(&path);
        match entry.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&target).to_human_error()?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(dir) = target.parent() {
                    std::fs::create_dir_all(dir).to_human_error()?;
                }
                let mut file = std::fs::File::create(&target).to_human_error()?;
                std::io::copy(&mut entry, &mut file).to_human_error()?;
            }
            other => {
                return Err(bad_bundle(format!(
                    "The revision bundle contains '{}', which is a {other:?} entry; only files and directories are allowed.",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").expect("write");
        std::fs::create_dir_all(dir.path().join("modules/motd")).expect("mkdir");
        std::fs::write(dir.path().join("modules/motd/module.yaml"), "name: motd\n").expect("write");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join(".git/HEAD"), "ref").expect("write");
        dir
    }

    #[test]
    fn packs_and_extracts_round_trip_without_hidden_files() {
        let src = tree();
        let bytes = pack_dir(src.path()).expect("pack");
        let out = tempfile::tempdir().expect("tempdir");
        let dest = out.path().join("revisions/abc");
        extract_atomic(&bytes, &dest).expect("extract");
        assert_eq!(
            std::fs::read_to_string(dest.join("modules/motd/module.yaml")).expect("read"),
            "name: motd\n"
        );
        assert!(!dest.join(".git").exists());
        let leftovers: Vec<_> = std::fs::read_dir(out.path().join("revisions"))
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["abc"], "no staging directories remain");
    }

    #[test]
    fn packing_is_deterministic_and_digests_track_content() {
        let src = tree();
        assert_eq!(
            pack_dir(src.path()).expect("pack"),
            pack_dir(src.path()).expect("pack")
        );
        let before = tree_digest(src.path()).expect("digest");
        std::fs::write(src.path().join(".git/HEAD"), "other").expect("write");
        assert_eq!(
            tree_digest(src.path()).expect("digest"),
            before,
            "hidden files do not count"
        );
        std::fs::write(src.path().join("buckle.yaml"), "version: 2\n# edit\n").expect("write");
        assert_ne!(tree_digest(src.path()).expect("digest"), before);
    }

    #[test]
    fn digest_verification_accepts_prefixed_digests_and_rejects_others() {
        let bytes = b"bundle";
        verify_digest(bytes, &format!("sha256:{}", sha256_hex(bytes))).expect("matches");
        verify_digest(bytes, &sha256_hex(bytes)).expect("matches");
        assert!(verify_digest(bytes, &sha256_hex(b"other")).is_err());
    }

    #[test]
    fn rejects_traversal_and_links() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "modules/evil", "/etc/passwd")
            .expect("append");
        let bytes = builder.into_inner().expect("tar").finish().expect("gzip");
        let out = tempfile::tempdir().expect("tempdir");
        let dest = out.path().join("r");
        assert!(extract_atomic(&bytes, &dest).is_err());
        assert!(!dest.exists());
    }
}
