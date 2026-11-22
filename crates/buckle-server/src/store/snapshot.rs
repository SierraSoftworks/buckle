//! Snapshots of the database around schema migrations, so that a server can
//! be downgraded without restoring a backup by hand.
//!
//! * **Upgrade**: before a migration changes an existing database, a copy of
//!   it is kept beside it as `buckle.db.schema-N` (N: the schema it was at).
//!   One is kept, the latest upgrade's.
//! * **Downgrade**: a server that meets a database at a newer schema than it
//!   knows puts that snapshot back and sets the newer database aside as
//!   `buckle.db.schema-M.set-aside`. What the newer server wrote is not in
//!   the snapshot: agents report their state again when they reconnect.
//! * **Refusal**: without a snapshot it can read, or with one older than
//!   [`RESTORE_WITHIN`] (going back to it would quietly drop that much
//!   history), the server does not start, and says what to do.

use super::{Restored, connect, db_error, schema};
use buckle_core::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How old a snapshot may be and still be put back without being asked:
/// long enough for an upgrade to be found bad and undone.
pub const RESTORE_WITHIN: Duration = Duration::from_secs(7 * 24 * 3600);

/// Readies the database at `path` for a server that knows `known`
/// migrations: snapshots it when migrations are about to change it, or puts
/// the snapshot back when it is from a newer server.
pub(super) fn prepare(path: &Path, known: i64) -> Result<Option<Restored>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = connect(path)?;
    let applied = schema(&conn)?;
    if applied == 0 || applied == known {
        return Ok(None);
    }
    if applied < known {
        // An older snapshot must never be taken for this upgrade's.
        for (_, old) in snapshots(path) {
            let _ = std::fs::remove_file(old);
        }
        for aside in set_aside(path) {
            let _ = std::fs::remove_file(aside);
        }
        let to = snapshot_path(path, applied);
        match take(&conn, &to) {
            Ok(()) => {
                tracing::info!(snapshot = %to.display(), schema = applied, "kept a snapshot of the database before migrating it")
            }
            // The upgrade goes ahead as it did before snapshots existed; a
            // downgrade then needs a backup.
            Err(e) => {
                tracing::error!(error = %e, snapshot = %to.display(), "the database could not be snapshotted before migrating it; a downgrade will need a backup of data_dir")
            }
        }
        return Ok(None);
    }
    let Some((to, snapshot)) = snapshots(path).into_iter().max_by_key(|(n, _)| *n) else {
        return Err(newer(
            path,
            applied,
            known,
            "no snapshot from before that upgrade is beside it",
        ));
    };
    if to > known {
        return Err(newer(
            path,
            applied,
            known,
            &format!(
                "the snapshot beside it is at schema {to}, which this server does not know either"
            ),
        ));
    }
    let age = std::fs::metadata(&snapshot)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .unwrap_or_default();
    if age > RESTORE_WITHIN {
        return Err(buckle_core::error::user(
            format!(
                "The database at {} is from a newer Buckle (schema {applied}; this server knows {known}), and the snapshot from before that upgrade ({}) is {} days old.",
                path.display(),
                snapshot.display(),
                age.as_secs() / 86400
            ),
            &[
                "Run the newer server again to keep what it recorded since.",
                "To go back to the snapshot and lose what was recorded since, stop the server, move the database aside and copy the snapshot in its place.",
            ],
        ));
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(db_error)?;
    drop(conn);
    let aside = with_suffix(path, &format!(".schema-{applied}.set-aside"));
    let io = |what: &str, e: std::io::Error| {
        buckle_core::error::wrap_system(
            e,
            format!(
                "The snapshot of the database from before the upgrade could not be restored: {what}."
            ),
            &["Check the data directory for disk space and permissions."],
        )
    };
    let staged = with_suffix(path, ".restoring");
    std::fs::copy(&snapshot, &staged).map_err(|e| io("copying it failed", e))?;
    std::fs::rename(path, &aside)
        .map_err(|e| io("the newer database could not be set aside", e))?;
    for sidecar in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(with_suffix(path, sidecar));
    }
    std::fs::rename(&staged, path).map_err(|e| io("it could not be moved into place", e))?;
    tracing::warn!(
        from = applied,
        to,
        set_aside = %aside.display(),
        "the database was from a newer Buckle: restored the snapshot taken before that upgrade"
    );
    Ok(Some(Restored {
        from: applied,
        to,
        set_aside: aside,
    }))
}

fn newer(path: &Path, applied: i64, known: i64, why: &str) -> buckle_core::Error {
    buckle_core::error::user(
        format!(
            "The database at {} is from a newer Buckle (schema {applied}; this server knows {known}), and {why}.",
            path.display()
        ),
        &["Run the newer server again, or restore a backup of data_dir from before the upgrade."],
    )
}

/// Copies the database behind `conn` to `to`, whole or not at all.
fn take(conn: &rusqlite::Connection, to: &Path) -> Result<()> {
    let staged = with_suffix(to, ".tmp");
    let _ = std::fs::remove_file(&staged);
    conn.execute("VACUUM INTO ?1", [staged.to_string_lossy().as_ref()])
        .map_err(db_error)?;
    std::fs::rename(&staged, to).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        buckle_core::error::wrap_system(
            e,
            "The database snapshot could not be moved into place.",
            &["Check the data directory for disk space and permissions."],
        )
    })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Where the snapshot of the database at `schema` is kept.
pub(super) fn snapshot_path(path: &Path, schema: i64) -> PathBuf {
    with_suffix(path, &format!(".schema-{schema}"))
}

/// The files beside the database whose name follows `<database>.schema-`.
fn siblings(path: &Path) -> Vec<(String, PathBuf)> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{name}.schema-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file = e.file_name().to_str()?.to_owned();
            let rest = file.strip_prefix(&prefix)?.to_owned();
            Some((rest, e.path()))
        })
        .collect()
}

/// The snapshots beside the database, by the schema each was taken at.
fn snapshots(path: &Path) -> Vec<(i64, PathBuf)> {
    siblings(path)
        .into_iter()
        .filter_map(|(rest, p)| Some((rest.parse().ok()?, p)))
        .collect()
}

/// Newer databases a restore set aside.
fn set_aside(path: &Path) -> Vec<PathBuf> {
    siblings(path)
        .into_iter()
        .filter(|(rest, _)| rest.ends_with(".set-aside"))
        .map(|(_, p)| p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{MIGRATIONS, Store};
    use super::*;

    /// A server one migration behind this one.
    fn older() -> &'static [&'static str] {
        &MIGRATIONS[..MIGRATIONS.len() - 1]
    }

    async fn note(store: &Store, key: &'static str) {
        store.call(move |db| db.kv().set(key, "1")).await.unwrap();
    }

    async fn noted(store: &Store, key: &'static str) -> bool {
        store
            .call(move |db| db.kv().get(key))
            .await
            .unwrap()
            .is_some()
    }

    /// A database an older server wrote `before` to, upgraded by this
    /// server, which wrote `after`.
    async fn upgraded(path: &Path) {
        note(&Store::open_with(path, older()).await.unwrap(), "before").await;
        let store = Store::open(path).await.unwrap();
        assert!(store.restored().is_none());
        note(&store, "after").await;
    }

    #[tokio::test]
    async fn an_upgrade_keeps_a_snapshot_a_downgrade_puts_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckle.db");
        upgraded(&path).await;
        assert!(snapshot_path(&path, older().len() as i64).exists());

        let store = Store::open_with(&path, older()).await.unwrap();
        let restored = store.restored().expect("the snapshot was put back");
        assert_eq!(
            (restored.from, restored.to),
            (MIGRATIONS.len() as i64, older().len() as i64)
        );
        assert!(restored.set_aside.exists(), "the newer database is kept");
        assert!(noted(&store, "before").await);
        assert!(
            !noted(&store, "after").await,
            "what the newer server wrote is not in the snapshot"
        );
        drop(store);

        // Upgrading again starts from the restored database, and an
        // ordinary restart restores nothing.
        let store = Store::open(&path).await.unwrap();
        assert!(store.restored().is_none());
        assert!(noted(&store, "before").await);
        drop(store);
        assert!(Store::open(&path).await.unwrap().restored().is_none());
    }

    #[tokio::test]
    async fn a_new_database_and_an_unchanged_schema_take_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckle.db");
        drop(Store::open(&path).await.unwrap());
        drop(Store::open(&path).await.unwrap());
        assert!(snapshots(&path).is_empty());
    }

    #[tokio::test]
    async fn a_downgrade_without_a_usable_snapshot_is_refused() {
        enum Snapshot {
            Missing,
            Stale,
        }
        for case in [Snapshot::Missing, Snapshot::Stale] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("buckle.db");
            upgraded(&path).await;
            let snapshot = snapshot_path(&path, older().len() as i64);
            let expect = match case {
                Snapshot::Missing => {
                    std::fs::remove_file(&snapshot).unwrap();
                    "no snapshot"
                }
                Snapshot::Stale => {
                    let at = SystemTime::now() - RESTORE_WITHIN - Duration::from_secs(3600);
                    std::fs::File::options()
                        .write(true)
                        .open(&snapshot)
                        .unwrap()
                        .set_modified(at)
                        .unwrap();
                    "days old"
                }
            };
            let error = Store::open_with(&path, older())
                .await
                .expect_err("the older server does not start")
                .to_string();
            assert!(error.contains(expect), "{error}");
            // The newer server still finds its database.
            let store = Store::open(&path).await.unwrap();
            assert!(noted(&store, "after").await);
        }
    }
}
