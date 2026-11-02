//! Rollouts and their per-host rows.

use super::{db_error, from_json, parse_ts, parse_ts_opt, to_json, ts};
use crate::rollout::model::HostPhase;
pub use crate::rollout::model::{RolloutHostRecord, RolloutRecord};
use buckle_api::RolloutState;
use buckle_core::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};

/// Repository over `rollouts` and `rollout_hosts`.
pub struct Rollouts<'c> {
    conn: &'c Connection,
}

pub(crate) fn state_str(s: RolloutState) -> String {
    match serde_json::to_value(s) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "detected".into(),
    }
}

fn parse_state(s: &str) -> Result<RolloutState> {
    from_json(&format!("\"{s}\""))
}

const R_COLUMNS: &str =
    "id, revision, state, policy, started_at, finished_at, triggered_by, detail";

type RawRollout = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
);

fn r_row(r: &Row<'_>) -> rusqlite::Result<RawRollout> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

fn r_record(raw: RawRollout) -> Result<RolloutRecord> {
    let (id, revision, state, policy, started, finished, triggered_by, detail) = raw;
    Ok(RolloutRecord {
        id,
        revision,
        state: parse_state(&state)?,
        policy: from_json(&policy)?,
        started_at: parse_ts(&started)?,
        finished_at: parse_ts_opt(finished)?,
        triggered_by,
        runtime: from_json(&detail)?,
    })
}

const H_COLUMNS: &str = "rollout, host, phase, wave, state, plan_digest, plan, result, health, created_at, updated_at, detail";

type RawHost = (
    String,
    String,
    String,
    Option<u32>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
);

fn h_row(r: &Row<'_>) -> rusqlite::Result<RawHost> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
    ))
}

fn h_record(raw: RawHost) -> Result<RolloutHostRecord> {
    let (rollout, host, phase, wave, state, digest, plan, result, health, created, updated, detail) =
        raw;
    Ok(RolloutHostRecord {
        rollout,
        host,
        phase,
        wave,
        state: HostPhase::parse(&state).ok_or_else(|| {
            buckle_core::error::system(
                format!("A rollout host row has an unknown state '{state}'."),
                &["The database may be from an incompatible Buckle version."],
            )
        })?,
        plan_digest: digest,
        plan: plan.as_deref().map(from_json).transpose()?,
        result: result.as_deref().map(from_json).transpose()?,
        health: health.as_deref().map(from_json).transpose()?,
        created_at: parse_ts(&created)?,
        updated_at: parse_ts(&updated)?,
        runtime: from_json(&detail)?,
    })
}

impl<'c> Rollouts<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Inserts or replaces the rollout row.
    pub fn save(&self, r: &RolloutRecord) -> Result<()> {
        self.conn
            .execute(
                &format!("INSERT INTO rollouts ({R_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(id) DO UPDATE SET state = excluded.state, policy = excluded.policy, finished_at = excluded.finished_at, detail = excluded.detail"),
                params![
                    r.id,
                    r.revision,
                    state_str(r.state),
                    to_json(&r.policy)?,
                    ts(r.started_at),
                    r.finished_at.map(ts),
                    r.triggered_by,
                    to_json(&r.runtime)?,
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The rollout, if known.
    pub fn get(&self, id: &str) -> Result<Option<RolloutRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {R_COLUMNS} FROM rollouts WHERE id = ?1"),
                params![id],
                r_row,
            )
            .optional()
            .map_err(db_error)?
            .map(r_record)
            .transpose()
    }

    /// Newest first.
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<RolloutRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {R_COLUMNS} FROM rollouts ORDER BY started_at DESC, rowid DESC LIMIT ?1 OFFSET ?2"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![limit, offset], r_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(r_record).collect()
    }

    /// Total number of rollouts.
    pub fn count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM rollouts", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(db_error)
    }

    /// Rollouts the controller must keep in memory: everything not terminal,
    /// plus completed rollouts whose stragglers stage is open. Oldest first.
    pub fn live(&self) -> Result<Vec<RolloutRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {R_COLUMNS} FROM rollouts WHERE state NOT IN ('rolled_back', 'aborted') ORDER BY started_at, rowid"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], r_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let records: Vec<RolloutRecord> = rows.into_iter().map(r_record).collect::<Result<_>>()?;
        Ok(records
            .into_iter()
            .filter(|r| !r.is_terminal() || r.runtime.stragglers_open)
            .collect())
    }

    /// The latest rollout of a revision.
    pub fn latest_for_revision(&self, revision: &str) -> Result<Option<RolloutRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {R_COLUMNS} FROM rollouts WHERE revision = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1"),
                params![revision],
                r_row,
            )
            .optional()
            .map_err(db_error)?
            .map(r_record)
            .transpose()
    }

    /// Inserts or replaces a host row.
    pub fn save_host(&self, h: &RolloutHostRecord) -> Result<()> {
        self.conn
            .execute(
                &format!("INSERT INTO rollout_hosts ({H_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT(rollout, host) DO UPDATE SET phase = excluded.phase, wave = excluded.wave, state = excluded.state, plan_digest = excluded.plan_digest, plan = excluded.plan, result = excluded.result, health = excluded.health, updated_at = excluded.updated_at, detail = excluded.detail"),
                params![
                    h.rollout,
                    h.host,
                    h.phase,
                    h.wave,
                    h.state.as_str(),
                    h.plan_digest,
                    h.plan.as_ref().map(to_json).transpose()?,
                    h.result.as_ref().map(to_json).transpose()?,
                    h.health.as_ref().map(to_json).transpose()?,
                    ts(h.created_at),
                    ts(h.updated_at),
                    to_json(&h.runtime)?,
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Every host row of a rollout, in insertion order.
    pub fn hosts(&self, rollout: &str) -> Result<Vec<RolloutHostRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {H_COLUMNS} FROM rollout_hosts WHERE rollout = ?1 ORDER BY rowid"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![rollout], h_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(h_record).collect()
    }

    /// Removes a host row (a forgotten host).
    pub fn delete_host(&self, rollout: &str, host: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM rollout_hosts WHERE rollout = ?1 AND host = ?2",
                params![rollout, host],
            )
            .map_err(db_error)?;
        Ok(())
    }
}

impl Rollouts<'_> {
    /// The revisions rollouts currently ask `host` to fetch: the target of
    /// every rollout that is not rolled back or aborted and has a row for the
    /// host (the rollout's revision, the host row's own target, which is the
    /// stable revision in the stragglers stage, and the previous stable
    /// revision a rollback sends it to). Rows that cannot be read are skipped.
    pub fn revisions_for_host(&self, host: &str) -> Result<std::collections::BTreeSet<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.id, r.revision, r.state, r.policy, r.started_at, r.finished_at, r.triggered_by, r.detail, h.detail \
                 FROM rollout_hosts h JOIN rollouts r ON r.id = h.rollout \
                 WHERE h.host = ?1 AND r.state NOT IN ('rolled_back', 'aborted')",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![host], |r| Ok((r_row(r)?, r.get::<_, String>(8)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let mut out = std::collections::BTreeSet::new();
        for (raw, host_detail) in rows {
            let id = raw.0.clone();
            let record = match r_record(raw) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(rollout = %id, error = %e, "skipping a rollout row that cannot be read");
                    continue;
                }
            };
            if record.is_terminal() && !record.runtime.stragglers_open {
                continue;
            }
            out.insert(record.revision.clone());
            if let Some(prev) = &record.runtime.previous_stable {
                out.insert(prev.clone());
            }
            if let Ok(rt) = from_json::<crate::rollout::model::HostRuntime>(&host_detail)
                && !rt.revision.is_empty()
            {
                out.insert(rt.revision);
            }
        }
        Ok(out)
    }

    /// The most recent healthy row for a host (what it last converged to).
    pub fn latest_healthy_for_host(&self, host: &str) -> Result<Option<RolloutHostRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {H_COLUMNS} FROM rollout_hosts WHERE host = ?1 AND state = 'healthy' ORDER BY updated_at DESC LIMIT 1"),
                params![host],
                h_row,
            )
            .optional()
            .map_err(db_error)?
            .map(h_record)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;
    use crate::rollout::model::{HostRuntime, RolloutRuntime};

    pub(crate) fn rollout(id: &str, revision: &str, state: RolloutState) -> RolloutRecord {
        RolloutRecord {
            id: id.into(),
            revision: revision.into(),
            state,
            policy: Default::default(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            triggered_by: "controller".into(),
            runtime: RolloutRuntime {
                previous_stable: Some("old".into()),
                ..Default::default()
            },
        }
    }

    pub(crate) fn host_row(rollout: &str, host: &str, revision: &str) -> RolloutHostRecord {
        RolloutHostRecord {
            rollout: rollout.into(),
            host: host.into(),
            phase: "all".into(),
            wave: None,
            state: HostPhase::PlanPending,
            plan_digest: None,
            plan: None,
            result: None,
            health: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            runtime: HostRuntime {
                revision: revision.into(),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn a_host_is_entitled_to_the_revisions_its_live_rollouts_name() {
        let store = Store::in_memory().await.unwrap();
        let revisions = store
            .call(|db| {
                db.rollouts()
                    .save(&rollout("r1", "new", RolloutState::Rolling))?;
                db.rollouts().save_host(&host_row("r1", "pi-1", "new"))?;
                db.rollouts()
                    .save(&rollout("r2", "dead", RolloutState::Aborted))?;
                db.rollouts().save_host(&host_row("r2", "pi-1", "dead"))?;
                db.rollouts()
                    .save(&rollout("r3", "other", RolloutState::Rolling))?;
                db.rollouts().save_host(&host_row("r3", "pi-2", "other"))?;
                db.rollouts().revisions_for_host("pi-1")
            })
            .await
            .unwrap();
        assert_eq!(
            revisions.into_iter().collect::<Vec<_>>(),
            vec!["new".to_string(), "old".to_string()]
        );
    }
}
