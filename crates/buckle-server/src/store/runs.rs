//! Apply runs per host, and the latest apply event per step of each run.
//!
//! A run is recorded from its first apply event as `running` (so a report
//! can show it live) and completed by the agent's ApplyResult, whose full
//! `ApplyOutcome` is kept as `outcome_json`. A result with no open run (its
//! events were all dropped, or it predates run reports) records a finished
//! run directly.

use super::{db_error, parse_ts, ts};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// The outcome of a run that has not finished.
pub const RUNNING: &str = "running";

/// The error of a run whose result will not come: the host started another
/// apply, reported another plan's result, or reconnected without it.
pub const ABANDONED: &str = "the run stopped before its result arrived: the host moved on to another apply or reconnected without this one";

/// One row of `runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// Stable identifier (`RunSummary.id`).
    pub id: String,
    pub host: String,
    pub rollout: Option<String>,
    pub revision: String,
    pub plan_digest: String,
    pub started_at: DateTime<Utc>,
    /// For a running run, when it started (until it finishes).
    pub finished_at: DateTime<Utc>,
    /// `running`, `succeeded`, `failed`, `needs_attention` or `stale_plan`.
    pub outcome: String,
    pub changes: u32,
    pub removals: u32,
    pub error: Option<String>,
    pub trace_id: Option<String>,
    /// The agent's `ApplyOutcome` as JSON, once the result arrived.
    pub outcome_json: Option<String>,
    /// For an operator's action, `module/action`; `None` for an apply.
    /// Action runs never count as the host's applies: they are not open,
    /// latest or bounding runs for anything apply-related.
    pub action: Option<String>,
}

impl RunRecord {
    /// A fresh run identifier.
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// Whether the run has not finished.
    pub fn is_running(&self) -> bool {
        self.outcome == RUNNING
    }
}

/// One stored apply event of a run (the latest for its key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEvent {
    /// The order the events were last written in.
    pub seq: i64,
    /// `buckle_core::engine::ApplyEvent` as JSON.
    pub event: String,
    pub at: DateTime<Utc>,
}

/// A one-off action run (outside any rollout) with who ran it, for the
/// fleet-wide listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRunRecord {
    pub run: RunRecord,
    /// The target's kind (`host`, `cluster`); empty for a forgotten target.
    pub kind: String,
    pub actor: Option<String>,
    pub label: Option<String>,
}

/// Repository over `runs` and `run_events`.
pub struct Runs<'c> {
    conn: &'c Connection,
}

const COLUMNS: &str = "run_id, host, rollout, revision, plan_digest, started_at, finished_at, outcome, changes, removals, error, trace_id, outcome_json, action";

fn row(r: &Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: r.get(0)?,
        host: r.get(1)?,
        rollout: r.get(2)?,
        revision: r.get(3)?,
        plan_digest: r.get(4)?,
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        outcome: r.get(7)?,
        changes: r.get(8)?,
        removals: r.get(9)?,
        error: r.get(10)?,
        trace_id: r.get(11)?,
        outcome_json: r.get(12)?,
        action: r.get(13)?,
    })
}

struct RawRun {
    id: Option<String>,
    host: String,
    rollout: Option<String>,
    revision: String,
    plan_digest: String,
    started_at: String,
    finished_at: String,
    outcome: String,
    changes: u32,
    removals: u32,
    error: Option<String>,
    trace_id: Option<String>,
    outcome_json: Option<String>,
    action: Option<String>,
}

impl RawRun {
    fn record(self) -> Result<RunRecord> {
        Ok(RunRecord {
            id: self.id.unwrap_or_default(),
            host: self.host,
            rollout: self.rollout,
            revision: self.revision,
            plan_digest: self.plan_digest,
            started_at: parse_ts(&self.started_at)?,
            finished_at: parse_ts(&self.finished_at)?,
            outcome: self.outcome,
            changes: self.changes,
            removals: self.removals,
            error: self.error,
            trace_id: self.trace_id,
            outcome_json: self.outcome_json,
            action: self.action,
        })
    }
}

impl<'c> Runs<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    fn query(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<RunRecord>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_error)?;
        let rows = stmt
            .query_map(args, row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(RawRun::record).collect()
    }

    /// Records a run (finished, or `running` from its first event). A run
    /// without an id gets one.
    pub fn add(&self, run: &RunRecord) -> Result<String> {
        let id = if run.id.is_empty() {
            RunRecord::new_id()
        } else {
            run.id.clone()
        };
        self.conn
            .execute(
                "INSERT INTO runs (run_id, host, rollout, revision, plan_digest, started_at, finished_at, outcome, changes, removals, error, trace_id, outcome_json, action) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    id,
                    run.host,
                    run.rollout,
                    run.revision,
                    run.plan_digest,
                    ts(run.started_at),
                    ts(run.finished_at),
                    run.outcome,
                    run.changes,
                    run.removals,
                    run.error,
                    run.trace_id,
                    run.outcome_json,
                    run.action,
                ],
            )
            .map_err(db_error)?;
        Ok(id)
    }

    /// Completes the run `run.id` with the result in `run` (its start and
    /// rollout are kept).
    pub fn finish(&self, run: &RunRecord) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET revision = ?2, plan_digest = ?3, finished_at = ?4, outcome = ?5, changes = ?6, removals = ?7, error = ?8, trace_id = COALESCE(?9, trace_id), outcome_json = ?10 WHERE run_id = ?1",
                params![
                    run.id,
                    run.revision,
                    run.plan_digest,
                    ts(run.finished_at),
                    run.outcome,
                    run.changes,
                    run.removals,
                    run.error,
                    run.trace_id,
                    run.outcome_json,
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Attaches a run to the rollout it belongs to.
    pub fn set_rollout(&self, run_id: &str, rollout: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET rollout = ?2 WHERE run_id = ?1",
                params![run_id, rollout],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The host's run that has not finished, if any (the newest one).
    pub fn open_for_host(&self, host: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND outcome = ?2 AND action IS NULL ORDER BY id DESC LIMIT 1"),
                params![host, RUNNING],
            )?
            .into_iter()
            .next())
    }

    /// The run a result for `plan_digest` completes: the host's open run for
    /// that plan, or one that was abandoned (another run started before its
    /// result arrived).
    pub fn unresolved(&self, host: &str, plan_digest: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND plan_digest = ?2 AND action IS NULL AND outcome_json IS NULL AND (outcome = ?3 OR error = ?4) ORDER BY id DESC LIMIT 1"),
                params![host, plan_digest, RUNNING, ABANDONED],
            )?
            .into_iter()
            .next())
    }

    /// Closes an open run whose result will not come (it may still arrive
    /// and complete it).
    pub fn abandon(&self, run_id: &str, at: DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET outcome = 'failed', error = ?2, finished_at = ?3 WHERE run_id = ?1 AND outcome = ?4",
                params![run_id, ABANDONED, ts(at), RUNNING],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Abandons the host's open run unless its revision is one of
    /// `unreported` (a new session's Hello lists the applies still running or
    /// whose result is on its way; any other open run's result will not
    /// come). Returns the run it closed.
    pub fn abandon_unless_unreported(
        &self,
        host: &str,
        unreported: &[String],
        at: DateTime<Utc>,
    ) -> Result<Option<RunRecord>> {
        let Some(open) = self.open_for_host(host)? else {
            return Ok(None);
        };
        if unreported.contains(&open.revision) {
            return Ok(None);
        }
        self.abandon(&open.id, at)?;
        self.get(host, &open.id)
    }

    /// One run of a host.
    pub fn get(&self, host: &str, run_id: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND run_id = ?2"),
                params![host, run_id],
            )?
            .into_iter()
            .next())
    }

    /// The most recent finished runs for a host, newest first.
    pub fn for_host(&self, host: &str, limit: u32) -> Result<Vec<RunRecord>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND outcome != ?3 ORDER BY finished_at DESC, id DESC LIMIT ?2"),
            params![host, limit, RUNNING],
        )
    }

    /// A page of a host's runs, running ones included, newest first: those
    /// recorded before the run `before` when it is given. Returns the page
    /// and the host's total.
    pub fn page(
        &self,
        host: &str,
        before: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<RunRecord>, u64)> {
        let items = match before {
            Some(before) => self.query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND id < COALESCE((SELECT id FROM runs WHERE host = ?1 AND run_id = ?2), 0) ORDER BY id DESC LIMIT ?3"),
                params![host, before, limit],
            )?,
            None => self.query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 ORDER BY id DESC LIMIT ?2"),
                params![host, limit],
            )?,
        };
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE host = ?1",
                params![host],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        Ok((items, total.max(0) as u64))
    }

    /// Records who ran the action run `run_id` and what the action is called.
    pub fn set_origin(&self, run_id: &str, actor: &str, label: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET actor = ?2, action_label = ?3 WHERE run_id = ?1",
                params![run_id, actor, label],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// A page of one-off action runs across every target (an action
    /// rollout's runs belong to its rollout), newest first, after the run
    /// `before` when given, and how many there are.
    pub fn actions(&self, before: Option<&str>, limit: u32) -> Result<(Vec<ActionRunRecord>, u64)> {
        let cols = COLUMNS
            .split(", ")
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols}, COALESCE(h.kind, ''), r.actor, r.action_label FROM runs r LEFT JOIN hosts h ON h.host_id = r.host \
             WHERE r.action IS NOT NULL AND r.rollout IS NULL \
             AND (?1 IS NULL OR r.id < COALESCE((SELECT id FROM runs WHERE run_id = ?1), 0)) \
             ORDER BY r.id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(db_error)?;
        let rows = stmt
            .query_map(params![before, limit], |r| {
                Ok((row(r)?, r.get::<_, String>(14)?, r.get(15)?, r.get(16)?))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let items = rows
            .into_iter()
            .map(|(raw, kind, actor, label)| {
                Ok(ActionRunRecord {
                    run: raw.record()?,
                    kind,
                    actor,
                    label,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE action IS NOT NULL AND rollout IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        Ok((items, total.max(0) as u64))
    }

    /// The newest apply run of a host (running or finished).
    pub fn latest_for_host(&self, host: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND action IS NULL ORDER BY id DESC LIMIT 1"),
                params![host],
            )?
            .into_iter()
            .next())
    }

    /// Whether an action is running on the host: `module/action` of it.
    pub fn running_action(&self, host: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT action FROM runs WHERE host = ?1 AND outcome = ?2 AND action IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![host, RUNNING],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    /// Closes the host's running action runs: a new session has none in
    /// flight (a durable result still completes them).
    pub fn abandon_actions(&self, host: &str, at: DateTime<Utc>) -> Result<Vec<RunRecord>> {
        let open = self.query(
            &format!(
                "SELECT {COLUMNS} FROM runs WHERE host = ?1 AND outcome = ?2 AND action IS NOT NULL"
            ),
            params![host, RUNNING],
        )?;
        let mut closed = Vec::new();
        for run in open {
            self.abandon(&run.id, at)?;
            if let Some(run) = self.get(host, &run.id)? {
                closed.push(run);
            }
        }
        Ok(closed)
    }

    /// The newest run of `host` that applied `revision`.
    pub fn latest_for_revision(&self, host: &str, revision: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND revision = ?2 AND action IS NULL ORDER BY id DESC LIMIT 1"),
                params![host, revision],
            )?
            .into_iter()
            .next())
    }

    /// The run recorded after `run` on the same host, if any (it bounds the
    /// health results that belong to `run`).
    pub fn next_after(&self, run: &RunRecord) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE host = ?1 AND action IS NULL AND id > (SELECT id FROM runs WHERE run_id = ?2) ORDER BY id ASC LIMIT 1"),
                params![run.host, run.id],
            )?
            .into_iter()
            .next())
    }

    /// The newest run of `host` in rollout `rollout`.
    pub fn latest_for_rollout_host(&self, rollout: &str, host: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE rollout = ?1 AND host = ?2 ORDER BY id DESC LIMIT 1"),
                params![rollout, host],
            )?
            .into_iter()
            .next())
    }

    /// The newest run of `host` in rollout `rollout` applying the plan
    /// `plan_digest` (a placement's own run: each placement of a cluster has
    /// a plan, and a run, of its own).
    pub fn latest_for_rollout_plan(
        &self,
        rollout: &str,
        host: &str,
        plan_digest: &str,
    ) -> Result<Option<RunRecord>> {
        Ok(self
            .query(
                &format!("SELECT {COLUMNS} FROM runs WHERE rollout = ?1 AND host = ?2 AND plan_digest = ?3 ORDER BY id DESC LIMIT 1"),
                params![rollout, host, plan_digest],
            )?
            .into_iter()
            .next())
    }

    /// The newest run id of every host of a rollout, and of every plan a
    /// host ran in it (a placement's run is its plan's).
    pub fn ids_for_rollout(&self, rollout: &str) -> Result<RolloutRunIds> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT host, plan_digest, run_id FROM runs WHERE rollout = ?1 ORDER BY id ASC",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![rollout], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let mut ids = RolloutRunIds::default();
        for (host, digest, id) in rows {
            let Some(id) = id else { continue };
            if let Some(d) = digest {
                ids.by_plan.insert((host.clone(), d), id.clone());
            }
            ids.by_host.insert(host, id);
        }
        Ok(ids)
    }

    /// Stores the latest event for one key (a step, module, handler,
    /// removal or check) of a run. A step stored as `failed` or `blocked`
    /// keeps that event: a module rollback runs the same step again, and the
    /// live report must still show what failed.
    #[allow(clippy::too_many_arguments)]
    pub fn record_event(
        &self,
        run_id: &str,
        key: &str,
        module: Option<&str>,
        step_id: Option<&str>,
        state: Option<&str>,
        event: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO run_events (run_id, key, seq, module, step_id, state, event, at) VALUES (?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1), ?3, ?4, ?5, ?6, ?7) ON CONFLICT(run_id, key) DO UPDATE SET seq = excluded.seq, module = excluded.module, step_id = excluded.step_id, state = excluded.state, event = excluded.event, at = excluded.at WHERE run_events.state IS NULL OR run_events.state NOT IN ('failed', 'blocked')",
                params![run_id, key, module, step_id, state, event, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// A run's stored events, in the order they were last written.
    pub fn events(&self, run_id: &str) -> Result<Vec<RunEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, event, at FROM run_events WHERE run_id = ?1 ORDER BY seq ASC")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(|(seq, event, at)| {
                Ok(RunEvent {
                    seq,
                    event,
                    at: parse_ts(&at)?,
                })
            })
            .collect()
    }

    /// Whether a run with this id exists.
    pub fn exists(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM runs WHERE run_id = ?1",
                params![run_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(db_error)?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use pretty_assertions::assert_eq;

    fn run(host: &str, outcome: &str, at: i64) -> RunRecord {
        let t = DateTime::from_timestamp(1_700_000_000 + at, 0).unwrap();
        RunRecord {
            id: String::new(),
            host: host.into(),
            rollout: Some("ro-1".into()),
            revision: "r1".into(),
            plan_digest: "d1".into(),
            started_at: t,
            finished_at: t,
            outcome: outcome.into(),
            changes: 0,
            removals: 0,
            error: None,
            trace_id: None,
            outcome_json: None,
            action: None,
        }
    }

    #[tokio::test]
    async fn a_running_run_is_found_finished_and_paged_newest_first() {
        let store = Store::in_memory().await.unwrap();
        let ids = store
            .call(|db| {
                let runs = db.runs();
                let old = runs.add(&run("pi-1", "succeeded", 0))?;
                let open = runs.add(&run("pi-1", RUNNING, 10))?;
                runs.add(&run("pi-2", RUNNING, 10))?;
                Ok((old, open))
            })
            .await
            .unwrap();
        let (old, open) = ids;
        assert_ne!(old, open);
        let (found, finished_only) = store
            .call(|db| {
                Ok((
                    db.runs().open_for_host("pi-1")?,
                    db.runs().for_host("pi-1", 10)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(found.as_ref().map(|r| r.id.clone()), Some(open.clone()));
        assert_eq!(finished_only.len(), 1, "for_host lists finished runs");

        let mut done = found.unwrap();
        done.outcome = "failed".into();
        done.error = Some("boom".into());
        done.outcome_json = Some("{\"succeeded\":false}".into());
        let o2 = open.clone();
        let (page, rest, got) = store
            .call(move |db| {
                db.runs().finish(&done)?;
                Ok((
                    db.runs().page("pi-1", None, 1)?,
                    db.runs().page("pi-1", Some(&o2), 10)?,
                    db.runs().get("pi-1", &o2)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(page.1, 2);
        assert_eq!(page.0[0].id, open, "newest first");
        assert_eq!(
            rest.0.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![old]
        );
        let got = got.unwrap();
        assert_eq!(got.outcome, "failed");
        assert_eq!(got.rollout.as_deref(), Some("ro-1"), "the rollout is kept");
        assert!(got.outcome_json.is_some());
    }

    #[tokio::test]
    async fn one_off_action_runs_are_listed_across_targets_newest_first() {
        let store = Store::in_memory().await.unwrap();
        let (page, rest) = store
            .call(|db| {
                let runs = db.runs();
                let action = |host: &str, key: &str, at: i64, rollout: Option<&str>| RunRecord {
                    rollout: rollout.map(Into::into),
                    action: Some(key.into()),
                    ..run(host, "succeeded", at)
                };
                runs.add(&run("pi-1", "succeeded", 0))?;
                let reboot = runs.add(&action("pi-1", "common/reboot", 10, None))?;
                runs.set_origin(&reboot, "alice", "Reboot")?;
                runs.add(&action("pi-2", "web/restart", 20, Some("ro-2")))?;
                runs.add(&action("pi-2", "web/drain", 30, None))?;
                let page = runs.actions(None, 1)?;
                let rest = runs.actions(Some(&page.0[0].run.id), 10)?;
                Ok((page, rest))
            })
            .await
            .unwrap();
        let keys = |p: &(Vec<ActionRunRecord>, u64)| {
            p.0.iter()
                .map(|r| r.run.action.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&page),
            vec!["web/drain"],
            "newest first, no applies or rollout runs"
        );
        assert_eq!(page.1, 2);
        assert_eq!(keys(&rest), vec!["common/reboot"]);
        assert_eq!(
            (rest.0[0].actor.as_deref(), rest.0[0].label.as_deref()),
            (Some("alice"), Some("Reboot"))
        );
    }

    #[tokio::test]
    async fn events_keep_the_latest_per_key_in_write_order() {
        let store = Store::in_memory().await.unwrap();
        let t = Utc::now();
        let events = store
            .call(move |db| {
                let runs = db.runs();
                runs.record_event("r", "module:m", Some("m"), None, None, "a", t)?;
                runs.record_event(
                    "r",
                    "step:m/x",
                    Some("m"),
                    Some("x"),
                    Some("running"),
                    "b",
                    t,
                )?;
                runs.record_event(
                    "r",
                    "step:m/x",
                    Some("m"),
                    Some("x"),
                    Some("changed"),
                    "c",
                    t,
                )?;
                runs.record_event("other", "step:m/x", None, None, None, "z", t)?;
                runs.events("r")
            })
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[tokio::test]
    async fn a_failed_or_blocked_step_is_never_replaced_by_a_later_event_of_it() {
        let store = Store::in_memory().await.unwrap();
        let t = Utc::now();
        let events = store
            .call(move |db| {
                let runs = db.runs();
                for (key, first) in [("step:m/x", "failed"), ("step:m/y", "blocked")] {
                    runs.record_event("r", key, Some("m"), None, Some("running"), "started", t)?;
                    runs.record_event("r", key, Some("m"), None, Some(first), first, t)?;
                    // The rollback runs it again.
                    runs.record_event("r", key, Some("m"), None, Some("running"), "again", t)?;
                    runs.record_event("r", key, Some("m"), None, Some("changed"), "again", t)?;
                }
                runs.events("r")
            })
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["failed", "blocked"]
        );
    }
}

/// The newest runs of a rollout's rows ([`Runs::ids_for_rollout`]).
#[derive(Debug, Default)]
pub struct RolloutRunIds {
    by_host: std::collections::BTreeMap<String, String>,
    by_plan: std::collections::BTreeMap<(String, String), String>,
}

impl RolloutRunIds {
    /// How many hosts (clusters counted once) have a run in the rollout.
    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    /// The run of `host` that applied plan `digest`.
    pub fn of_plan(&self, host: &str, digest: &str) -> Option<String> {
        self.by_plan
            .get(&(host.to_owned(), digest.to_owned()))
            .cloned()
    }

    /// The run a rollout row is on: a placement's is the run of its own
    /// plan (a cluster runs one per placement), a host's its newest.
    pub fn get(&self, host: &str, workload: Option<&str>, plan: Option<&str>) -> Option<String> {
        match (workload, plan) {
            (Some(_), Some(d)) => self.by_plan.get(&(host.to_owned(), d.to_owned())).cloned(),
            (Some(_), None) => None,
            (None, _) => self.by_host.get(host).cloned(),
        }
    }
}
