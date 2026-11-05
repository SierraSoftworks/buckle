//! Hosts: enrolment, labels, status, connection state and latest facts.
//!
//! Cluster targets live in the same table with `kind = 'cluster'` and their
//! spec in `cluster`, so status, labels, notes, health and drift work alike
//! for both. [`Hosts::get`] and [`Hosts::list`] see hosts only; the
//! `target`/`targets`/`clusters` methods see every kind.

use super::{db_error, from_json, parse_ts, parse_ts_opt, to_json, ts};
use buckle_api::{ClusterInfo, Connection as LinkState, HealthSummary, HostStatus, TargetKind};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::BTreeMap;

/// One row of `hosts`: a host, or a cluster target.
#[derive(Debug, Clone, PartialEq)]
pub struct HostRecord {
    pub id: String,
    pub kind: TargetKind,
    /// The cluster half of a cluster target; `None` for a host.
    pub cluster: Option<ClusterInfo>,
    /// For a host, the labels its agent reports; for a cluster, the labels
    /// its declaration (or creation) seeded.
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
    /// For a cluster: the host agent holding its lease (its steward).
    pub steward: Option<String>,
    /// For a cluster: whether its API answers the steward, as last reported;
    /// `None` until a steward reports.
    pub reachable: Option<bool>,
    /// Why the cluster is unreachable, when it is.
    pub reachable_error: Option<String>,
}

/// `auth_method` of a host enrolled by workload identity: the prefix, then
/// the cluster it executes for.
pub const WORKLOAD_AUTH_PREFIX: &str = "workload_identity:";

impl HostRecord {
    /// For an executor running inside a cluster, the cluster it speaks for.
    /// Such a host runs no modules of its own; it only executes for that
    /// cluster and the targets scoped to it.
    pub fn executor_of(&self) -> Option<&str> {
        self.auth_method
            .as_deref()
            .and_then(|m| m.strip_prefix(WORKLOAD_AUTH_PREFIX))
    }

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

/// What is needed to create a cluster target.
#[derive(Debug, Clone)]
pub struct NewCluster {
    pub id: String,
    pub cluster: ClusterInfo,
    /// Seed labels: the declaration's, or the creating request's.
    pub agent_labels: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub notes: String,
    pub status: HostStatus,
    pub at: DateTime<Utc>,
}

/// Repository over `hosts` and `host_facts`.
pub struct Hosts<'c> {
    conn: &'c Connection,
}

const COLUMNS: &str = "host_id, agent_labels, labels, status, notes, token_hash, platform, arch, agent_version, applied_revision, last_plan_digest, connection, auth_method, first_seen, last_seen, health, health_at, drifted, kind, cluster, steward, reachable, reachable_error";

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
        kind: r.get(18)?,
        cluster: r.get(19)?,
        steward: r.get(20)?,
        reachable: r.get(21)?,
        reachable_error: r.get(22)?,
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
    kind: String,
    cluster: Option<String>,
    steward: Option<String>,
    reachable: Option<String>,
    reachable_error: Option<String>,
}

impl RawHost {
    fn into_record(self) -> Result<HostRecord> {
        Ok(HostRecord {
            id: self.id,
            kind: if self.kind == "cluster" {
                TargetKind::Cluster
            } else {
                TargetKind::Host
            },
            cluster: self.cluster.map(|c| from_json(&c)).transpose()?,
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
            steward: self.steward.filter(|s| !s.is_empty()),
            reachable: match self.reachable.as_deref() {
                Some("reachable") => Some(true),
                Some("unreachable") => Some(false),
                _ => None,
            },
            reachable_error: self.reachable_error.filter(|e| !e.is_empty()),
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

    /// Creates a cluster target. Fails if a target of that id exists.
    pub fn create_cluster(&self, cluster: &NewCluster) -> Result<HostRecord> {
        self.conn
            .execute(
                "INSERT INTO hosts (host_id, kind, cluster, agent_labels, labels, notes, status, first_seen) VALUES (?1, 'cluster', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    cluster.id,
                    to_json(&cluster.cluster)?,
                    to_json(&cluster.agent_labels)?,
                    to_json(&cluster.labels)?,
                    cluster.notes,
                    status_str(cluster.status),
                    ts(cluster.at)
                ],
            )
            .map_err(db_error)?;
        self.target(&cluster.id)?.ok_or_else(|| {
            buckle_core::error::system("A cluster target vanished right after it was created.", &[])
        })
    }

    /// The host, if known. A cluster target of that id is not a host.
    pub fn get(&self, id: &str) -> Result<Option<HostRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM hosts WHERE host_id = ?1 AND kind = 'host'"),
                params![id],
                row,
            )
            .optional()
            .map_err(db_error)?
            .map(RawHost::into_record)
            .transpose()
    }

    /// The target (host or cluster) of this id, if known.
    pub fn target(&self, id: &str) -> Result<Option<HostRecord>> {
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

    /// Every host, by id (cluster targets excluded).
    pub fn list(&self) -> Result<Vec<HostRecord>> {
        self.select("WHERE kind = 'host'")
    }

    /// Every target of every kind, by id.
    pub fn targets(&self) -> Result<Vec<HostRecord>> {
        self.select("")
    }

    /// Every cluster target, by id.
    pub fn clusters(&self) -> Result<Vec<HostRecord>> {
        self.select("WHERE kind = 'cluster'")
    }

    fn select(&self, filter: &str) -> Result<Vec<HostRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM hosts {filter} ORDER BY host_id"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(RawHost::into_record).collect()
    }

    /// Replaces a cluster target's spec and seed labels (a declaration that
    /// changed, or an operator's edit).
    pub fn set_cluster(
        &self,
        id: &str,
        cluster: &ClusterInfo,
        agent_labels: &BTreeMap<String, String>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE hosts SET cluster = ?2, agent_labels = ?3 WHERE host_id = ?1 AND kind = 'cluster'",
                params![id, to_json(cluster)?, to_json(agent_labels)?],
            )
            .map_err(db_error)?;
        Ok(n > 0)
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

    /// Records which host agent holds a cluster target's lease (`None`:
    /// nobody). A cluster without a steward is neither reachable nor not:
    /// nobody is asking.
    pub fn set_steward(&self, id: &str, steward: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET steward = ?2, reachable = CASE WHEN ?2 IS NULL THEN NULL ELSE reachable END, reachable_error = CASE WHEN ?2 IS NULL THEN NULL ELSE reachable_error END WHERE host_id = ?1",
                params![id, steward],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Records whether a cluster target's API answers its steward.
    pub fn set_reachable(&self, id: &str, reachable: bool, error: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET reachable = ?2, reachable_error = ?3 WHERE host_id = ?1",
                params![
                    id,
                    if reachable {
                        "reachable"
                    } else {
                        "unreachable"
                    },
                    error.filter(|_| !reachable)
                ],
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

    /// Forgets the host, its facts, its drift, its health and the reports it
    /// sent.
    pub fn delete(&self, id: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM health_events WHERE host = ?1", params![id])
            .map_err(db_error)?;
        self.conn
            .execute("DELETE FROM health_status WHERE host = ?1", params![id])
            .map_err(db_error)?;
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
    async fn cluster_targets_share_the_table_but_not_the_host_views() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let spec = buckle_api::ClusterSpec {
            kind: buckle_api::ClusterKind::Nomad,
            description: String::new(),
            api: None,
            ui: None,
            nodes: Some("host.groups contains \"nomad\"".into()),
            executors: None,
            identity: buckle_api::ClusterSpec::default_identity(),
            cluster: None,
            scope: Default::default(),
            trust: None,
            watch: None,
        };
        let (host, target, hosts, targets, clusters) = store
            .call(move |db| {
                db.hosts().create(&NewHost {
                    id: "pi-1".into(),
                    status: HostStatus::Active,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: now,
                })?;
                db.hosts().create_cluster(&NewCluster {
                    id: "home-nomad".into(),
                    cluster: ClusterInfo {
                        spec: spec.clone(),
                        declared: true,
                    },
                    agent_labels: [("site".to_string(), "home".to_string())].into(),
                    labels: BTreeMap::new(),
                    notes: String::new(),
                    status: HostStatus::Active,
                    at: now,
                })?;
                db.hosts()
                    .set_health("home-nomad", HealthSummary::Healthy, now)?;
                Ok((
                    db.hosts().get("home-nomad")?,
                    db.hosts().target("home-nomad")?,
                    db.hosts().list()?,
                    db.hosts().targets()?,
                    db.hosts().clusters()?,
                ))
            })
            .await
            .unwrap();
        assert!(host.is_none(), "a cluster is not a host");
        let target = target.expect("the cluster is a target");
        assert_eq!(target.kind, TargetKind::Cluster);
        assert_eq!(
            target.cluster.as_ref().map(|c| c.spec.kind),
            Some(buckle_api::ClusterKind::Nomad)
        );
        assert!(target.cluster.as_ref().is_some_and(|c| c.declared));
        assert_eq!(target.effective_labels()["site"], "home");
        assert_eq!(target.health, HealthSummary::Healthy);
        assert_eq!(
            hosts.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["pi-1"]
        );
        assert_eq!(targets.len(), 2);
        assert_eq!(clusters.len(), 1);
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
