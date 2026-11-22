//! The units an operator suspended: a module on a host, or a workload on a
//! cluster target. A suspended unit is left alone by rollouts, drift
//! correction and actions until it is resumed.

use super::{db_error, parse_ts, ts};
use buckle_api::PausedUnit;
use buckle_core::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// Repository over `paused_units`.
pub struct PausedUnits<'c> {
    conn: &'c Connection,
}

const COLUMNS: &str = "target, unit, note, actor, at, review_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(PausedUnit, String, Option<String>)> {
    Ok((
        PausedUnit {
            target: r.get(0)?,
            unit: r.get(1)?,
            note: r.get(2)?,
            by: r.get(3)?,
            at: Default::default(),
            review_at: None,
        },
        r.get(4)?,
        r.get(5)?,
    ))
}

fn unit((mut unit, at, review): (PausedUnit, String, Option<String>)) -> Result<PausedUnit> {
    unit.at = parse_ts(&at)?;
    unit.review_at = review.as_deref().map(parse_ts).transpose()?;
    Ok(unit)
}

impl<'c> PausedUnits<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Suspends a unit, or replaces what a suspended one says.
    pub fn set(&self, unit: &PausedUnit) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO paused_units (target, unit, note, actor, at, review_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (target, unit) DO UPDATE SET note = excluded.note, actor = excluded.actor, at = excluded.at, review_at = excluded.review_at",
                params![
                    unit.target,
                    unit.unit,
                    unit.note,
                    unit.by,
                    ts(unit.at),
                    unit.review_at.map(ts)
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Resumes a unit; whether it was suspended.
    pub fn remove(&self, target: &str, unit: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM paused_units WHERE target = ?1 AND unit = ?2",
                params![target, unit],
            )
            .map_err(db_error)?;
        Ok(n > 0)
    }

    pub fn get(&self, target: &str, name: &str) -> Result<Option<PausedUnit>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM paused_units WHERE target = ?1 AND unit = ?2"),
                params![target, name],
                row,
            )
            .optional()
            .map_err(db_error)?
            .map(unit)
            .transpose()
    }

    /// Every suspended unit, by target and name.
    pub fn list(&self) -> Result<Vec<PausedUnit>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM paused_units ORDER BY target, unit"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(unit).collect()
    }

    /// The units suspended on `target`, by name.
    pub fn of(&self, target: &str) -> Result<Vec<PausedUnit>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|u| u.target == target)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::store::{NewHost, Store};
    use buckle_api::{HostStatus, PausedUnit};
    use chrono::{Duration, SubsecRound, Utc};

    fn unit(target: &str, name: &str, note: &str) -> PausedUnit {
        PausedUnit {
            target: target.into(),
            unit: name.into(),
            note: note.into(),
            by: "alice".into(),
            at: Utc::now().trunc_subsecs(0),
            review_at: None,
        }
    }

    #[tokio::test]
    async fn units_are_kept_per_target_until_resumed_or_the_target_is_forgotten() {
        let store = Store::in_memory().await.unwrap();
        let review = Utc::now().trunc_subsecs(0) + Duration::days(2);
        let (names, again, of_pi, removed, left) = store
            .call(move |db| {
                db.hosts().create(&NewHost {
                    id: "pi-1".into(),
                    status: HostStatus::Active,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: Utc::now(),
                })?;
                let units = db.paused();
                units.set(&unit("pi-1", "web", "disk swap"))?;
                units.set(&unit("home-nomad", "app", "cve"))?;
                // Suspending again replaces what it says.
                units.set(&PausedUnit {
                    review_at: Some(review),
                    ..unit("pi-1", "web", "still swapping")
                })?;
                let names: Vec<(String, String)> = units
                    .list()?
                    .into_iter()
                    .map(|u| (u.target, u.unit))
                    .collect();
                let again = units.get("pi-1", "web")?;
                let of_pi = units.of("pi-1")?.len();
                let removed = (
                    units.remove("home-nomad", "app")?,
                    units.remove("home-nomad", "app")?,
                );
                db.hosts().delete("pi-1")?;
                Ok((names, again, of_pi, removed, db.paused().list()?))
            })
            .await
            .unwrap();
        assert_eq!(
            names,
            vec![
                ("home-nomad".to_owned(), "app".to_owned()),
                ("pi-1".to_owned(), "web".to_owned())
            ]
        );
        let again = again.expect("suspended");
        assert_eq!(
            (again.note.as_str(), again.review_at),
            ("still swapping", Some(review))
        );
        assert_eq!(of_pi, 1);
        assert_eq!(removed, (true, false));
        assert!(left.is_empty(), "{left:?}");
    }
}
