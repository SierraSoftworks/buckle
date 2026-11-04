//! Cluster targets on the server: the clusters `buckle.yaml` declares are
//! kept as rows beside the hosts (so status, labels, notes, health and drift
//! work for them too), with the repository's spec and seed labels winning
//! over the row's whenever the reference revision is read. Operators create
//! further clusters through `POST /api/v1/targets`.

use crate::store::{Db, HostRecord, NewCluster};
use buckle_api::{ClusterInfo, ClusterSpec, HostStatus, TargetKind};
use buckle_core::Filter;
use buckle_core::engine::Revision;
use buckle_core::model::{ClusterDecl, ClusterIdentity};
use buckle_core::selectors::selector_namespaces;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// The API's view of a `fleet.clusters` entry.
pub fn spec_from_decl(decl: &ClusterDecl) -> ClusterSpec {
    ClusterSpec {
        kind: api_kind(decl.kind),
        description: decl.description.clone(),
        api: decl.api.clone(),
        nodes: decl.nodes.as_ref().map(|f| f.raw().to_owned()),
        executors: decl.executors.as_ref().map(|f| f.raw().to_owned()),
        identity: decl.identity.to_string(),
        cluster: decl.cluster.clone(),
        scope: buckle_api::ClusterScope {
            namespaces: decl.scope.namespaces.clone(),
        },
    }
}

pub fn api_kind(kind: buckle_core::model::ClusterKind) -> buckle_api::ClusterKind {
    match kind {
        buckle_core::model::ClusterKind::Nomad => buckle_api::ClusterKind::Nomad,
        buckle_core::model::ClusterKind::Kubernetes => buckle_api::ClusterKind::Kubernetes,
    }
}

pub fn core_kind(kind: buckle_api::ClusterKind) -> buckle_core::model::ClusterKind {
    match kind {
        buckle_api::ClusterKind::Nomad => buckle_core::model::ClusterKind::Nomad,
        buckle_api::ClusterKind::Kubernetes => buckle_core::model::ClusterKind::Kubernetes,
    }
}

/// Why a cluster spec cannot be accepted, as the operator should read it.
/// `targets` are the existing targets (for the parent of a scoped target)
/// and `revision` the reference revision (for a `secret://` identity).
pub fn check_spec(
    id: &str,
    spec: &ClusterSpec,
    targets: &[HostRecord],
    revision: Option<&Revision>,
) -> Result<(), String> {
    for (key, selector) in [("nodes", &spec.nodes), ("executors", &spec.executors)] {
        if let Some(raw) = selector {
            Filter::new(raw.as_str())
                .map_err(|e| format!("`{key}` is not a valid selector: {e}"))?;
            if selector_namespaces(raw).cluster {
                return Err(format!(
                    "`{key}` selects on `cluster.*`, but it chooses the hosts that belong to the cluster; select on `host.*` instead."
                ));
            }
        }
    }
    let identity = ClusterIdentity::parse(&spec.identity)
        .map_err(|e| format!("`identity` is not valid: {e}"))?;
    if let Some(secret) = identity.secret()
        && let Some(rev) = revision
        && !rev.fleet.secrets.contains_key(secret)
    {
        return Err(format!(
            "`identity` names the secret '{secret}', which buckle.yaml does not declare under `secrets`."
        ));
    }
    match &spec.cluster {
        None if !spec.scope.is_empty() => Err(
            "`scope` needs `cluster`: a scope narrows a parent cluster, so name the cluster it is a part of.".to_owned(),
        ),
        None => Ok(()),
        Some(parent) if parent == id => Err("A target cannot be scoped to itself.".to_owned()),
        Some(parent) => match targets.iter().find(|t| t.id == *parent) {
            None => Err(format!("The cluster '{parent}' does not exist.")),
            Some(t) => match &t.cluster {
                None => Err(format!("'{parent}' is a host, not a cluster.")),
                Some(c) if c.spec.cluster.is_some() => Err(format!(
                    "'{parent}' is itself a scoped target; scope the cluster it belongs to instead."
                )),
                Some(c) if c.spec.kind != spec.kind => Err(format!(
                    "'{parent}' is a {} cluster, so a target scoped to it must be one too.",
                    c.spec.kind
                )),
                Some(_) => Ok(()),
            },
        },
    }
}

/// Brings the cluster rows in line with what `revision` declares: a
/// declared cluster without a row is created (active, with the
/// declaration's labels as its seed labels), a row whose spec or seed labels
/// differ is updated, and a row that was declared but no longer is becomes
/// an operator-owned target. Returns the ids that changed.
pub fn sync_declared(
    db: &Db<'_>,
    revision: &Revision,
    now: DateTime<Utc>,
) -> buckle_core::Result<Vec<String>> {
    let existing = db.hosts().clusters()?;
    let declared = &revision.fleet.fleet.clusters;
    let mut changed = Vec::new();
    for (name, decl) in declared {
        let info = ClusterInfo {
            spec: spec_from_decl(decl),
            declared: true,
        };
        let seed = decl.text_labels();
        match existing.iter().find(|r| r.id == *name) {
            Some(r) if r.cluster.as_ref() == Some(&info) && r.agent_labels == seed => {}
            Some(_) => {
                db.hosts().set_cluster(name, &info, &seed)?;
                changed.push(name.clone());
            }
            None => {
                if db.hosts().target(name)?.is_some() {
                    tracing::warn!(
                        buckle.target = %name,
                        "buckle.yaml declares a cluster named like an existing host; the host keeps the name"
                    );
                    continue;
                }
                db.hosts().create_cluster(&NewCluster {
                    id: name.clone(),
                    cluster: info,
                    agent_labels: seed,
                    labels: BTreeMap::new(),
                    notes: String::new(),
                    status: HostStatus::Active,
                    at: now,
                })?;
                changed.push(name.clone());
            }
        }
    }
    for r in &existing {
        let Some(info) = &r.cluster else { continue };
        if info.declared && !declared.contains_key(&r.id) {
            let mut info = info.clone();
            info.declared = false;
            db.hosts().set_cluster(&r.id, &info, &r.agent_labels)?;
            changed.push(r.id.clone());
        }
    }
    Ok(changed)
}

/// Whether `record` is a cluster target.
pub fn is_cluster(record: &HostRecord) -> bool {
    record.kind == TargetKind::Cluster
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use buckle_core::plan::RevisionId;
    use std::sync::Arc;

    fn revision(yaml: &str) -> Revision {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), yaml).unwrap();
        buckle_core::engine::loader::load_dir(dir.path(), RevisionId::new("r")).unwrap()
    }

    const DECLARED: &str = "version: 2
fleet:
  clusters:
    home-nomad:
      kind: nomad
      nodes: 'host.groups contains \"nomad\"'
      labels: { site: home }
secrets:
  NOMAD: { env: NOMAD }
";

    #[tokio::test]
    async fn declared_clusters_are_materialised_and_follow_the_repository() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let rev = Arc::new(revision(DECLARED));
        let (changed, rows) = {
            let rev = rev.clone();
            store
                .call(move |db| {
                    let changed = sync_declared(db, &rev, now)?;
                    Ok((changed, db.hosts().clusters()?))
                })
                .await
                .unwrap()
        };
        assert_eq!(changed, vec!["home-nomad"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_labels["site"], "home");
        assert!(rows[0].cluster.as_ref().unwrap().declared);
        assert_eq!(rows[0].status, HostStatus::Active);

        // A second sync changes nothing; a changed declaration is applied
        // and operator labels survive.
        let again = {
            let rev = rev.clone();
            store
                .call(move |db| {
                    let mut ops = BTreeMap::new();
                    ops.insert("tier".to_string(), "gold".to_string());
                    db.hosts().set_labels("home-nomad", &ops)?;
                    sync_declared(db, &rev, now)
                })
                .await
                .unwrap()
        };
        assert!(again.is_empty());
        let rev2 = Arc::new(revision(&DECLARED.replace("site: home", "site: lab")));
        let rows = store
            .call(move |db| {
                assert_eq!(sync_declared(db, &rev2, now)?, vec!["home-nomad"]);
                db.hosts().clusters()
            })
            .await
            .unwrap();
        assert_eq!(rows[0].agent_labels["site"], "lab");
        assert_eq!(rows[0].labels["tier"], "gold");

        // Dropped from the repository: the row stays, operator-owned.
        let rev3 = Arc::new(revision("version: 2\n"));
        let rows = store
            .call(move |db| {
                assert_eq!(sync_declared(db, &rev3, now)?, vec!["home-nomad"]);
                db.hosts().clusters()
            })
            .await
            .unwrap();
        assert!(!rows[0].cluster.as_ref().unwrap().declared);
    }

    fn spec(kind: buckle_api::ClusterKind) -> ClusterSpec {
        ClusterSpec {
            kind,
            description: String::new(),
            api: None,
            nodes: Some("host.labels.nomad == true".into()),
            executors: None,
            identity: ClusterSpec::default_identity(),
            cluster: None,
            scope: Default::default(),
        }
    }

    #[test]
    fn specs_are_checked_before_they_are_stored() {
        let rev = revision(DECLARED);
        let ok = spec(buckle_api::ClusterKind::Nomad);
        assert_eq!(check_spec("c", &ok, &[], Some(&rev)), Ok(()));

        let mut bad = ok.clone();
        bad.nodes = Some("cluster.labels.x == 1".into());
        assert!(
            check_spec("c", &bad, &[], None)
                .unwrap_err()
                .contains("cluster.*")
        );

        let mut bad = ok.clone();
        bad.nodes = Some("host.labels.x ==".into());
        assert!(
            check_spec("c", &bad, &[], None)
                .unwrap_err()
                .contains("not a valid selector")
        );

        let mut bad = ok.clone();
        bad.identity = "secret://NOPE".into();
        assert!(
            check_spec("c", &bad, &[], Some(&rev))
                .unwrap_err()
                .contains("NOPE")
        );
        bad.identity = "secret://NOMAD".into();
        assert_eq!(check_spec("c", &bad, &[], Some(&rev)), Ok(()));

        let mut scoped = spec(buckle_api::ClusterKind::Kubernetes);
        scoped.cluster = Some("nope".into());
        assert!(
            check_spec("c", &scoped, &[], None)
                .unwrap_err()
                .contains("does not exist")
        );
        scoped.cluster = Some("c".into());
        assert!(
            check_spec("c", &scoped, &[], None)
                .unwrap_err()
                .contains("itself")
        );
    }
}
