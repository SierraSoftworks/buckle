//! The agent's at-rest copy of the secrets assigned to it, so that a restart
//! (or a drift check while the server is unreachable) can re-render files
//! without asking the server again.
//!
//! **The values are plaintext on disk, by design.** The cache holds exactly
//! the values that the agent has already rendered into files on the same
//! host (`/etc/nomad.d/nomad.hcl`, credentials files and so on), so it is at
//! the same trust level as those files; encrypting it with a key stored next
//! to it would add ceremony, not protection. It is written with mode `0600`
//! (owner read/write only) from the moment it is created, atomically (a
//! temporary file in the same directory, then a rename), and belongs in the
//! agent's `state_dir`.
//!
//! The file is CBOR: an envelope with a format name, a version, the payload
//! (the CBOR-encoded entries: name, fingerprint, value) and an HMAC-SHA256
//! tag over the payload under the agent's [`InstallKey`]. The tag detects a
//! truncated, bit-flipped or foreign cache, all of which are system errors;
//! fingerprints are stored rather than recomputed, so they stay the ones the
//! server assigned.

use super::sinks::Sink;
use super::{Fingerprint, InstallKey, SecretStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FORMAT: &str = "buckle-secret-cache";
const VERSION: u32 = 1;
const MAC_DOMAIN: &[u8] = b"buckle-secret-cache-v1\0";

/// Reads and writes the agent's secret cache. See the module docs.
///
/// ```
/// use buckle_core::secrets::cache::SecretCache;
/// use buckle_core::secrets::sinks::TransportSink;
/// use buckle_core::secrets::{InstallKey, Secret, SecretStore};
///
/// let dir = tempfile::tempdir().unwrap();
/// let path = dir.path().join("secrets.cbor");
/// let key = InstallKey::from_bytes([7u8; 32]);
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("db", "hunter2".into(), &key));
///
/// SecretCache::save(&path, &store, &key, &TransportSink).unwrap();
/// let back = SecretCache::load(&path, &key).unwrap().expect("cache exists");
/// assert_eq!(back.get("db"), store.get("db"));
/// assert!(SecretCache::load(&dir.path().join("absent"), &key).unwrap().is_none());
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct SecretCache;

/// Bytes serialised as a CBOR byte string rather than an array of integers.
struct Bytes(Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(v))
            }
        }
        d.deserialize_byte_buf(Visitor)
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    version: u32,
    payload: Bytes,
    mac: Bytes,
}

/// One cached secret. Deliberately not `Debug`.
#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    fingerprint: Fingerprint,
    value: Bytes,
}

fn corrupt(path: &Path, why: &str) -> crate::Error {
    crate::error::system(
        format!(
            "The secret cache at {} is corrupt or unreadable: {why}.",
            path.display()
        ),
        &[
            "Delete the file; the agent fetches its secrets from the server again on the next assignment.",
        ],
    )
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl SecretCache {
    /// Writes `store` to `path` atomically with mode `0600`, tagged under `key`.
    ///
    /// The values leave their wrappers through `sink` (pass
    /// [`super::sinks::TransportSink`]).
    pub fn save(
        path: &Path,
        store: &SecretStore,
        key: &InstallKey,
        sink: &dyn Sink,
    ) -> crate::Result<()> {
        let entries: Vec<Entry> = store
            .export(sink)
            .into_iter()
            .map(|(name, fingerprint, value)| Entry {
                name,
                fingerprint,
                value: Bytes(value),
            })
            .collect();
        let mut payload = Vec::new();
        ciborium::into_writer(&entries, &mut payload).map_err(|e| {
            crate::error::wrap_system(
                e,
                "The secret cache could not be encoded.",
                &["This is a bug in Buckle; please report it."],
            )
        })?;
        drop(entries);
        let mac = key.mac(MAC_DOMAIN, &payload).to_vec();
        let envelope = Envelope {
            format: FORMAT.to_owned(),
            version: VERSION,
            payload: Bytes(payload),
            mac: Bytes(mac),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&envelope, &mut bytes).map_err(|e| {
            crate::error::wrap_system(
                e,
                "The secret cache could not be encoded.",
                &["This is a bug in Buckle; please report it."],
            )
        })?;
        drop(envelope);
        write_private_atomic(path, &bytes)
    }

    /// Reads the cache at `path`, verifying its tag under `key`.
    ///
    /// `Ok(None)` when there is no cache. A cache that does not parse, has an
    /// unknown version, or whose tag does not verify (corruption, or a file
    /// written under another install key) is a system error.
    pub fn load(path: &Path, key: &InstallKey) -> crate::Result<Option<SecretStore>> {
        use crate::error::HumanizableResult;
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).to_human_error(),
        };
        let envelope: Envelope = ciborium::from_reader(bytes.as_slice())
            .map_err(|_| corrupt(path, "it is not a valid cache file"))?;
        if envelope.format != FORMAT {
            return Err(corrupt(path, "it is not a Buckle secret cache"));
        }
        if envelope.version != VERSION {
            return Err(corrupt(
                path,
                &format!("it has unsupported version {}", envelope.version),
            ));
        }
        let expected = key.mac(MAC_DOMAIN, &envelope.payload.0);
        if !constant_time_eq(&expected, &envelope.mac.0) {
            return Err(corrupt(
                path,
                "its integrity check failed (it was modified, truncated or written under another install key)",
            ));
        }
        let entries: Vec<Entry> = ciborium::from_reader(envelope.payload.0.as_slice())
            .map_err(|_| corrupt(path, "its entries could not be decoded"))?;
        SecretStore::import_fingerprinted(
            entries
                .into_iter()
                .map(|e| (e.name, e.fingerprint, e.value.0)),
        )
        .map(Some)
    }
}

/// Writes `bytes` to a new `0600` temporary file next to `path`, syncs it and
/// renames it into place, so readers see the old or the new cache, never a
/// partial one, and the content is never readable by others.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    use crate::error::HumanizableResult;
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).to_human_error()?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "secrets".to_owned());
    let temp: PathBuf = parent.join(format!(
        ".{file_name}.tmp.{}.{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        {
            // Make the rename durable; failure to sync a directory is not fatal.
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.to_human_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secret;
    use crate::secrets::sinks::TransportSink;

    fn key() -> InstallKey {
        InstallKey::from_bytes([4u8; 32])
    }

    fn store() -> SecretStore {
        let mut store = SecretStore::new();
        store.insert(Secret::new("a", "alpha-value".into(), &key()));
        store.insert(Secret::new("b", "".into(), &key()));
        store
    }

    #[test]
    fn round_trips_names_fingerprints_and_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secrets.cbor");
        let original = store();
        SecretCache::save(&path, &original, &key(), &TransportSink).unwrap();
        let back = SecretCache::load(&path, &key()).unwrap().unwrap();
        assert_eq!(back.names().collect::<Vec<_>>(), vec!["a", "b"]);
        let exported = back.export(&TransportSink);
        assert_eq!(exported, original.export(&TransportSink));
        // No temporary files are left behind.
        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn keeps_fingerprints_assigned_by_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.cbor");
        let server_key = InstallKey::from_bytes([9u8; 32]);
        let mut assigned = SecretStore::new();
        assigned.insert(Secret::new("a", "v".into(), &server_key));
        SecretCache::save(&path, &assigned, &key(), &TransportSink).unwrap();
        let back = SecretCache::load(&path, &key()).unwrap().unwrap();
        assert_eq!(
            back.get("a").unwrap().fingerprint(),
            assigned.get("a").unwrap().fingerprint()
        );
    }

    #[test]
    fn overwrites_an_existing_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.cbor");
        SecretCache::save(&path, &store(), &key(), &TransportSink).unwrap();
        SecretCache::save(&path, &SecretStore::new(), &key(), &TransportSink).unwrap();
        assert!(
            SecretCache::load(&path, &key())
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn is_written_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.cbor");
        SecretCache::save(&path, &store(), &key(), &TransportSink).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn missing_cache_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            SecretCache::load(&dir.path().join("nope"), &key())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn corruption_and_foreign_keys_are_system_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.cbor");
        SecretCache::save(&path, &store(), &key(), &TransportSink).unwrap();
        let good = std::fs::read(&path).unwrap();

        let mut cases: Vec<(&str, Vec<u8>)> = vec![
            ("garbage", b"not cbor at all".to_vec()),
            ("empty", Vec::new()),
            ("truncated", good[..good.len() / 2].to_vec()),
        ];
        // Flip a byte inside the value "alpha-value".
        let pos = good
            .windows(b"alpha-value".len())
            .position(|w| w == b"alpha-value")
            .unwrap();
        let mut flipped = good.clone();
        flipped[pos] ^= 0x01;
        cases.push(("flipped", flipped));

        for (what, bytes) in cases {
            std::fs::write(&path, &bytes).unwrap();
            let err = SecretCache::load(&path, &key()).err().unwrap_or_else(|| {
                panic!("{what}: expected an error");
            });
            assert!(err.is(crate::error::Kind::System), "{what}");
            assert!(!err.message().contains("alpha-value"), "{what}");
        }

        std::fs::write(&path, &good).unwrap();
        let err = SecretCache::load(&path, &InstallKey::from_bytes([5u8; 32]))
            .expect_err("a foreign key fails verification");
        assert!(err.is(crate::error::Kind::System));
    }
}
