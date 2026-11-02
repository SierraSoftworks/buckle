//! Hosts: enrolment, labels, status, connection state and latest facts.

use super::{db_error, from_json, parse_ts, parse_ts_opt, to_json, ts};
use buckle_api::{Connection as LinkState, HealthSummary, HostStatus};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::BTreeMap;

/// One row of `hosts`.
#[derive(Debug, Clone, PartialEq)]
pub struct HostRecord {
    pub id: String,
    pub agent_labels: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub status: HostStatus,
    pub notes: String,
    pub token_hash: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub agent_version: Option<String>,
    pub applied_revision: Option<String>,
    pub last_plan_digest: Option<String>,
    pub connection: LinkState,
    pub auth_method: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
    pub health: HealthSummary,
    pub health_at: Option<DateTime<Utc>>,
    pub drifted: bool,
}

impl HostRecord {
    /// Agent labels overlaid with server labels (server labels win).
    pub fn effective_labels(&self) -> BTreeMap<String, String> {
        let mut out = self.agent_labels.clone();
        for (k, v) in &self.labels {
            out.insert(k.clone(), v.clone());
        }
        out
    }
}

/// What is needed to create a host.
#[derive(Debug, Clone)]
pub struct NewHost {
    pub id: String,
    pub status: HostStatus,
    pub token_hash: Option<String>,
    pub auth_method: String,
    pub at: DateTime<Utc>,
}

/// Repository over `hosts` and `host_facts`.
pub struct Hosts<'c> {
    conn: &'c Connection,
}

const COLUMNS: &str = "host_id, agent_labels, labels, status, notes, token_hash, platform, arch, agent_version, applied_revision, last_plan_digest, connection, auth_method, first_seen, last_seen, health, health_at, drifted";

pub(crate) fn status_str(s: HostStatus) -> &'static str {
    match s {
        HostStatus::Pending => "pending",
        HostStatus::Active => "active",
        HostStatus::Paused => "paused",
    }
}

fn parse_status(s: &str) -> HostStatus {
    match s {
        "active" => HostStatus::Active,
        "paused" => HostStatus::Paused,
        _ => HostStatus::Pending,
    }
}

pub(crate) fn health_str(h: HealthSummary) -> &'static str {
    match h {
        HealthSummary::Healthy => "healthy",
        HealthSummary::Failing => "failing",
        HealthSummary::Held => "held",
        HealthSummary::Unknown => "unknown",
    }
}

fn parse_health(s: &str) -> HealthSummary {
    match s {
        "healthy" => HealthSummary::Healthy,
        "failing" => HealthSummary::Failing,
        "held" => HealthSummary::Held,
        _ => HealthSummary::Unknown,
    }
}

fn row(r: &Row<'_>) -> rusqlite::Result<RawHost> {
    Ok(RawHost {
        id: r.get(0)?,
        agent_labels: r.get(1)?,
        labels: r.get(2)?,
        status: r.get(3)?,
        notes: r.get(4)?,
        token_hash: r.get(5)?,
        platform: r.get(6)?,
        arch: r.get(7)?,
        agent_version: r.get(8)?,
        applied_revision: r.get(9)?,
        last_plan_digest: r.get(10)?,
        connection: r.get(11)?,
        auth_method: r.get(12)?,
        first_seen: r.get(13)?,
        last_seen: r.get(14)?,
        health: r.get(15)?,
        health_at: r.get(16)?,
        drifted: r.get(17)?,
    })
}

struct RawHost {
    id: String,
    agent_labels: String,
    labels: String,
    status: String,
    notes: String,
    token_hash: Option<String>,
    platform: Option<String>,
    arch: Option<String>,
    agent_version: Option<String>,
    applied_revision: Option<String>,
    last_plan_digest: Option<String>,
    connection: String,
    auth_method: Option<String>,
    first_seen: String,
    last_seen: Option<String>,
    health: String,
    health_at: Option<String>,
    drifted: bool,
}

impl RawHost {
    fn into_record(self) -> Result<HostRecord> {
        Ok(HostRecord {
            id: self.id,
            agent_labels: from_json(&self.agent_labels)?,
            labels: from_json(&self.labels)?,
            status: parse_status(&self.status),
            notes: self.notes,
            token_hash: self.token_hash,
            platform: self.platform,
            arch: self.arch,
            agent_version: self.agent_version,
            applied_revision: self.applied_revision,
            last_plan_digest: self.last_plan_digest,
            connection: if self.connection == "online" {
                LinkState::Online
            } else {
                LinkState::Offline
            },
            auth_method: self.auth_method,
            first_seen: parse_ts(&self.first_seen)?,
            last_seen: parse_ts_opt(self.last_seen)?,
            health: parse_health(&self.health),
            health_at: parse_ts_opt(self.health_at)?,
            drifted: self.drifted,
        })
    }
}

impl<'c> Hosts<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Creates a host. Fails if it exists.
    pub fn create(&self, host: &NewHost) -> Result<HostRecord> {
        self.conn
            .execute(
                "INSERT INTO hosts (host_id, status, token_hash, auth_method, first_seen) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host.id, status_str(host.status), host.token_hash, host.auth_method, ts(host.at)],
            )
            .map_err(db_error)?;
        self.get(&host.id)?.ok_or_else(|| {
            buckle_core::error::system("A host vanished right after it was created.", &[])
        })
    }

    /// The host, if known.
    pub fn get(&self, id: &str) -> Result<Option<HostRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM hosts WHERE host_id = ?1"),
                params![id],
                row,
            )
            .optional()
            .map_err(db_error)?
            .map(RawHost::into_record)
            .transpose()
    }

    /// The host owning the token with this SHA-256 hash.
    pub fn by_token_hash(&self, hash: &str) -> Result<Option<HostRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM hosts WHERE token_hash = ?1"),
                params![hash],
                row,
            )
            .optional()
            .map_err(db_error)?
            .map(RawHost::into_record)
            .transpose()
    }

    /// Every host, by id.
    pub fn list(&self) -> Result<Vec<HostRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM hosts ORDER BY host_id"))
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(RawHost::into_record).collect()
    }

    /// Records a Hello: version, platform, seed labels and connection.
    #[allow(clippy::too_many_arguments)]
    pub fn record_hello(
        &self,
        id: &str,
        agent_version: &str,
        platform: &str,
        arch: &str,
        agent_labels: &BTreeMap<String, String>,
        applied_revision: Option<&str>,
        last_plan_digest: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let none_if_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        self.conn
            .execute(
                "UPDATE hosts SET agent_version = ?2, platform = COALESCE(?3, platform), arch = COALESCE(?4, arch), agent_labels = ?5, applied_revision = COALESCE(?6, applied_revision), last_plan_digest = COALESCE(?7, last_plan_digest), connection = 'online', last_seen = ?8 WHERE host_id = ?1",
                params![
                    id,
                    agent_version,
                    none_if_empty(platform),
                    none_if_empty(arch),
                    to_json(agent_labels)?,
                    applied_revision.and_then(none_if_empty),
                    last_plan_digest.and_then(none_if_empty),
                    ts(at)
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Marks the host online or offline.
    pub fn set_connection(&self, id: &str, state: LinkState, at: DateTime<Utc>) -> Result<()> {
        let s = match state {
            LinkState::Online => "online",
            LinkState::Offline => "offline",
        };
        self.conn
            .execute(
                "UPDATE hosts SET connection = ?2, last_seen = ?3 WHERE host_id = ?1",
                params![id, s, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Marks every host offline (server start).
    pub fn mark_all_offline(&self) -> Result<()> {
        self.conn
            .execute("UPDATE hosts SET connection = 'offline'", [])
            .map_err(db_error)?;
        Ok(())
    }

    /// Updates liveness.
    pub fn touch(&self, id: &str, at: DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET last_seen = ?2 WHERE host_id = ?1",
                params![id, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Sets the issued per-host token's hash.
    pub fn set_token_hash(&self, id: &str, hash: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET token_hash = ?2 WHERE host_id = ?1",
                params![id, hash],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Sets the registration status.
    pub fn set_status(&self, id: &str, status: HostStatus) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE hosts SET status = ?2 WHERE host_id = ?1",
                params![id, status_str(status)],
            )
            .map_err(db_error)?;
        Ok(n > 0)
    }

    /// Replaces the server-managed labels.
    pub fn set_labels(&self, id: &str, labels: &BTreeMap<String, String>) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE hosts SET labels = ?2 WHERE host_id = ?1",
                params![id, to_json(labels)?],
            )
            .map_err(db_error)?;
        Ok(n > 0)
    }

    /// Replaces the operator notes.
    pub fn set_notes(&self, id: &str, notes: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE hosts SET notes = ?2 WHERE host_id = ?1",
                params![id, notes],
            )
            .map_err(db_error)?;
        Ok(n > 0)
    }

    /// Records the applied revision.
    pub fn set_applied_revision(&self, id: &str, revision: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET applied_revision = ?2 WHERE host_id = ?1",
                params![id, revision],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Records the latest plan digest.
    pub fn set_last_plan_digest(&self, id: &str, digest: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET last_plan_digest = ?2 WHERE host_id = ?1",
                params![id, digest],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Records the health roll-up.
    pub fn set_health(&self, id: &str, health: HealthSummary, at: DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET health = ?2, health_at = ?3 WHERE host_id = ?1",
                params![id, health_str(health), ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Records whether the last drift pass found changes.
    pub fn set_drifted(&self, id: &str, drifted: bool) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET drifted = ?2 WHERE host_id = ?1",
                params![id, drifted],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Forgets the host, its facts, its drift and the reports it sent.
    pub fn delete(&self, id: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM host_facts WHERE host_id = ?1", params![id])
            .map_err(db_error)?;
        self.conn
            .execute("DELETE FROM host_drift WHERE host_id = ?1", params![id])
            .map_err(db_error)?;
        self.conn
            .execute("DELETE FROM agent_reports WHERE host = ?1", params![id])
            .map_err(db_error)?;
        let n = self
            .conn
            .execute("DELETE FROM hosts WHERE host_id = ?1", params![id])
            .map_err(db_error)?;
        Ok(n > 0)
    }

    /// Stores a facts document and prunes history older than seven days.
    pub fn record_facts(
        &self,
        id: &str,
        facts: &serde_json::Value,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO host_facts (host_id, facts, collected_at) VALUES (?1, ?2, ?3)",
                params![id, to_json(facts)?, ts(at)],
            )
            .map_err(db_error)?;
        let cutoff = at - chrono::Duration::days(7);
        self.conn
            .execute(
                "DELETE FROM host_facts WHERE host_id = ?1 AND collected_at < ?2",
                params![id, ts(cutoff)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The latest facts and when they were collected.
    pub fn latest_facts(&self, id: &str) -> Result<Option<(serde_json::Value, DateTime<Utc>)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT facts, collected_at FROM host_facts WHERE host_id = ?1 ORDER BY collected_at DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        row.map(|(f, at)| Ok((from_json(&f)?, parse_ts(&at)?)))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;

    #[tokio::test]
    async fn hosts_enrol_with_a_token_and_are_found_by_its_hash() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let found = store
            .call(move |db| {
                db.hosts().create(&NewHost {
                    id: "pi-1".into(),
                    status: HostStatus::Pending,
                    token_hash: Some("abc".into()),
                    auth_method: "token".into(),
                    at: now,
                })?;
                db.hosts().set_status("pi-1", HostStatus::Active)?;
                let mut labels = BTreeMap::new();
                labels.insert("role".to_string(), "worker".to_string());
                db.hosts().record_hello(
                    "pi-1", "2.0.0", "linux", "aarch64", &labels, None, None, now,
                )?;
                let mut server = BTreeMap::new();
                server.insert("role".to_string(), "canary".to_string());
                db.hosts().set_labels("pi-1", &server)?;
                Ok((
                    db.hosts().by_token_hash("abc")?,
                    db.hosts().by_token_hash("nope")?,
                ))
            })
            .await
            .unwrap();
        let host = found.0.expect("token hash finds the host");
        assert_eq!(host.id, "pi-1");
        assert_eq!(host.status, HostStatus::Active);
        assert_eq!(host.connection, LinkState::Online);
        assert_eq!(host.effective_labels()["role"], "canary");
        assert_eq!(host.agent_labels["role"], "worker");
        assert!(found.1.is_none());
    }

    #[tokio::test]
    async fn token_hashes_are_unique() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let result = store
            .call(move |db| {
                for id in ["a", "b"] {
                    db.hosts().create(&NewHost {
                        id: id.into(),
                        status: HostStatus::Active,
                        token_hash: Some("same".into()),
                        auth_method: "token".into(),
                        at: now,
                    })?;
                }
                Ok(())
            })
            .await;
        assert!(result.is_err());
    }
}
