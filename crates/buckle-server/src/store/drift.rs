//! What the latest drift pass found different on each host (design §04
//! "Drift", §07): one row per drifted resource, replaced by every drift
//! report and cleared once the host is back in line.

use super::{db_error, parse_ts, ts};
use buckle_api::DriftItem;
use buckle_core::Result;
use buckle_core::plan::{Diff, Plan, Planned};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// Repository over `host_drift` (and the `hosts.drifted` flag it explains).
pub struct Drift<'c> {
    conn: &'c Connection,
}

/// The drifted resources a drift plan describes: every step that would
/// change something, then every removal. A resource reported again keeps
/// the time it was first seen.
pub fn items_of(plan: &Plan) -> Vec<DriftItem> {
    let steps = plan.steps().filter_map(|s| {
        let Planned::Change { diff } = &s.planned else {
            return None;
        };
        let unified = match diff {
            Diff::Update {
                unified: Some(u), ..
            } => Some(u.get().clone()),
            _ => None,
        };
        Some(DriftItem {
            module: s.resource.module.clone(),
            kind: s.resource.kind.clone(),
            key: s.resource.key.clone(),
            summary: diff.summary().to_owned(),
            unified,
            detected_at: None,
        })
    });
    let removals = plan.removals.iter().map(|r| DriftItem {
        module: r.resource.module.clone(),
        kind: r.resource.kind.clone(),
        key: r.resource.key.clone(),
        summary: format!("no longer declared: {}", r.summary),
        unified: None,
        detected_at: None,
    });
    steps.chain(removals).collect()
}

impl<'c> Drift<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records a drift report: whether the host has drifted and, when it
    /// has, what the report's plan (`buckle_core::plan::Plan` as JSON) says
    /// differs. A report that found nothing, or corrected what it found,
    /// clears the items. A plan that cannot be read leaves the flag set
    /// with no items rather than failing the report.
    pub fn record(
        &self,
        host: &str,
        drifted: bool,
        plan_json: &[u8],
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET drifted = ?2 WHERE host_id = ?1",
                params![host, drifted],
            )
            .map_err(db_error)?;
        let items = if drifted {
            match serde_json::from_slice::<Plan>(plan_json) {
                Ok(plan) => items_of(&plan),
                Err(e) => {
                    tracing::warn!(buckle.host = %host, error = %e, "a drift report's plan could not be read");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        self.replace(host, &items, at)
    }

    /// Forgets the host's drift: an apply succeeded, so what it applied is
    /// what the host now has.
    pub fn clear(&self, host: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE hosts SET drifted = 0 WHERE host_id = ?1",
                params![host],
            )
            .map_err(db_error)?;
        self.conn
            .execute("DELETE FROM host_drift WHERE host_id = ?1", params![host])
            .map_err(db_error)?;
        Ok(())
    }

    fn replace(&self, host: &str, items: &[DriftItem], at: DateTime<Utc>) -> Result<()> {
        let existing: Vec<(String, String, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT module, kind, key FROM host_drift WHERE host_id = ?1")
                .map_err(db_error)?;
            stmt.query_map(params![host], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(db_error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db_error)?
        };
        for (module, kind, key) in &existing {
            let still = items
                .iter()
                .any(|i| (&i.module, &i.kind, &i.key) == (module, kind, key));
            if !still {
                self.conn
                    .execute(
                        "DELETE FROM host_drift WHERE host_id = ?1 AND module = ?2 AND kind = ?3 AND key = ?4",
                        params![host, module, kind, key],
                    )
                    .map_err(db_error)?;
            }
        }
        for item in items {
            self.conn
                .execute(
                    "INSERT INTO host_drift (host_id, module, kind, key, summary, unified, detected_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                     ON CONFLICT(host_id, module, kind, key) DO UPDATE SET summary = excluded.summary, unified = excluded.unified",
                    params![host, item.module, item.kind, item.key, item.summary, item.unified, ts(at)],
                )
                .map_err(db_error)?;
        }
        Ok(())
    }

    /// The host's drifted resources, in module, kind and key order.
    pub fn for_host(&self, host: &str) -> Result<Vec<DriftItem>> {
        let mut stmt = self
            .conn
            .prepare("SELECT module, kind, key, summary, unified, detected_at FROM host_drift WHERE host_id = ?1 ORDER BY module, kind, key")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![host], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(|(module, kind, key, summary, unified, at)| {
                Ok(DriftItem {
                    module,
                    kind,
                    key,
                    summary,
                    unified,
                    detected_at: Some(parse_ts(&at)?),
                })
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{NewHost, Store};
    use super::*;
    use buckle_api::HostStatus;
    use buckle_core::model::RemovalPolicy;
    use buckle_core::plan::{ModulePlan, Removal, ResourceKey, RevisionId, Step, TaskRef};
    use buckle_core::secrets::Redacted;

    fn step(module: &str, kind: &str, key: &str, planned: Planned) -> Step {
        Step {
            task: TaskRef {
                module: module.into(),
                id: key.into(),
                kind: kind.into(),
                name: None,
            },
            resource: ResourceKey::new(module, kind, key),
            planned,
            locks: vec![],
            notifies: vec![],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
            facts: None,
        }
    }

    /// A drift plan: a hand-edited file, a changed sysctl, an unchanged
    /// unit, and a resource the revision no longer declares.
    pub(crate) fn drift_plan(swappiness: bool) -> Plan {
        let mut steps = vec![
            step(
                "app",
                "file.copy",
                "/etc/app.conf",
                Planned::change(Diff::unified(
                    "1 line changed",
                    &Redacted::plain("edited by hand\n".to_string()),
                    &Redacted::plain("managed\n".to_string()),
                )),
            ),
            step("app", "systemd.unit", "app", Planned::NoOp),
        ];
        if swappiness {
            steps.push(step(
                "common",
                "sysctl.param",
                "vm.swappiness",
                Planned::change(Diff::Opaque {
                    summary: "60 → 1".into(),
                }),
            ));
        }
        Plan::new(
            RevisionId::new("r1"),
            "pi-1",
            vec![ModulePlan {
                module: "app".into(),
                needs: vec![],
                steps,
                handlers: vec![],
                provides_facts: vec![],
            }],
            vec![Removal {
                resource: ResourceKey::new("grey", "file.copy", "/etc/grey"),
                policy: RemovalPolicy::Cleanup,
                summary: "delete /etc/grey".into(),
                caution: false,
                introduced_in: None,
            }],
            vec![],
        )
    }

    async fn store_with_host() -> Store {
        let store = Store::in_memory().await.unwrap();
        store
            .call(|db| {
                db.hosts().create(&NewHost {
                    id: "pi-1".into(),
                    status: HostStatus::Active,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: Utc::now(),
                })?;
                Ok(())
            })
            .await
            .unwrap();
        store
    }

    async fn state(store: &Store) -> (bool, Vec<DriftItem>) {
        store
            .call(|db| {
                let host = db.hosts().get("pi-1")?.expect("host");
                Ok((host.drifted, db.drift().for_host("pi-1")?))
            })
            .await
            .unwrap()
    }

    #[test]
    fn a_drift_plan_names_each_changed_resource_and_removal() {
        let items = items_of(&drift_plan(true));
        let keys: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.kind.as_str(), i.key.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("file.copy", "/etc/app.conf"),
                ("sysctl.param", "vm.swappiness"),
                ("file.copy", "/etc/grey"),
            ],
            "no-op steps are not drift"
        );
        let edited = &items[0];
        assert_eq!(edited.summary, "1 line changed");
        let unified = edited.unified.as_deref().expect("a unified diff");
        assert!(unified.contains("-edited by hand") && unified.contains("+managed"));
        assert_eq!(items[1].unified, None);
        assert_eq!(items[2].summary, "no longer declared: delete /etc/grey");
    }

    #[tokio::test]
    async fn drift_is_replaced_by_each_report_and_cleared_when_resolved() {
        let store = store_with_host().await;
        let first = Utc::now() - chrono::Duration::minutes(15);
        let plan = serde_json::to_vec(&drift_plan(true)).unwrap();
        store
            .call(move |db| db.drift().record("pi-1", true, &plan, first))
            .await
            .unwrap();
        let (drifted, items) = state(&store).await;
        assert!(drifted);
        assert_eq!(items.len(), 3);

        // The next pass still finds the file, no longer the sysctl.
        let later = Utc::now();
        let plan = serde_json::to_vec(&drift_plan(false)).unwrap();
        store
            .call(move |db| db.drift().record("pi-1", true, &plan, later))
            .await
            .unwrap();
        let (_, items) = state(&store).await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.key != "vm.swappiness"));
        let file = items.iter().find(|i| i.key == "/etc/app.conf").unwrap();
        assert_eq!(
            file.detected_at.map(|t| t.timestamp_micros()),
            Some(first.timestamp_micros()),
            "a resource still drifted keeps when it was first seen"
        );

        // A pass that finds nothing (or corrected it) clears everything.
        store
            .call(move |db| db.drift().record("pi-1", false, &[], later))
            .await
            .unwrap();
        assert_eq!(state(&store).await, (false, vec![]));

        // So does a successful apply.
        let plan = serde_json::to_vec(&drift_plan(true)).unwrap();
        store
            .call(move |db| {
                db.drift().record("pi-1", true, &plan, later)?;
                db.drift().clear("pi-1")
            })
            .await
            .unwrap();
        assert_eq!(state(&store).await, (false, vec![]));
    }

    #[tokio::test]
    async fn an_unreadable_plan_keeps_the_flag_without_items() {
        let store = store_with_host().await;
        store
            .call(|db| db.drift().record("pi-1", true, b"{not a plan", Utc::now()))
            .await
            .unwrap();
        assert_eq!(state(&store).await, (true, vec![]));
    }

    #[tokio::test]
    async fn forgetting_a_host_forgets_its_drift() {
        let store = store_with_host().await;
        let plan = serde_json::to_vec(&drift_plan(true)).unwrap();
        let left = store
            .call(move |db| {
                db.drift().record("pi-1", true, &plan, Utc::now())?;
                db.hosts().delete("pi-1")?;
                db.drift().for_host("pi-1")
            })
            .await
            .unwrap();
        assert!(left.is_empty());
    }
}
