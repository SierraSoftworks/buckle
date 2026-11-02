//! Host views shared by the gRPC and HTTP fronts.

use crate::source::cache::RevisionCache;
use crate::store::{HostRecord, Store, kv::keys};
use buckle_api::HostSummary;
use buckle_core::engine::Revision;
use std::sync::Arc;

/// The stable revision, loaded, when there is one.
pub async fn stable_revision(store: &Store, revisions: &RevisionCache) -> Option<Arc<Revision>> {
    let id = store
        .call(|db| db.kv().get(keys::STABLE_REVISION))
        .await
        .ok()
        .flatten()?;
    revisions.load(&id).await.ok()
}

/// The newest valid revision (for evaluating groups before anything is stable).
pub async fn reference_revision(store: &Store, revisions: &RevisionCache) -> Option<Arc<Revision>> {
    if let Some(r) = stable_revision(store, revisions).await {
        return Some(r);
    }
    let list = store.call(|db| db.revisions().list(20, 0)).await.ok()?;
    let valid = list
        .into_iter()
        .find(|r| r.status == buckle_api::RevisionStatus::Valid)?;
    revisions.load(&valid.id).await.ok()
}

/// The summary row of a host, with groups evaluated against `fleet`.
pub fn host_summary(
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: Option<&Revision>,
) -> HostSummary {
    let groups = fleet
        .map(|r| crate::fleet::record_context(host, facts, &r.fleet).groups)
        .unwrap_or_default();
    HostSummary {
        id: host.id.clone(),
        status: host.status,
        connection: host.connection,
        agent_labels: host.agent_labels.clone(),
        labels: host.labels.clone(),
        effective_labels: host.effective_labels(),
        groups,
        platform: host.platform.clone(),
        arch: host.arch.clone(),
        agent_version: host.agent_version.clone(),
        applied_revision: host.applied_revision.clone(),
        health: host.health,
        health_at: host.health_at,
        drifted: host.drifted,
        first_seen: host.first_seen,
        last_seen: host.last_seen,
        notes: host.notes.clone(),
    }
}

/// Loads and summarises one host.
pub async fn load_summary(
    store: &Store,
    revisions: &RevisionCache,
    id: &str,
) -> Option<HostSummary> {
    let id = id.to_owned();
    let (host, facts) = store
        .call(move |db| {
            let Some(h) = db.hosts().get(&id)? else {
                return Ok(None);
            };
            let facts = db.hosts().latest_facts(&id)?.map(|(f, _)| f);
            Ok(Some((h, facts.unwrap_or(serde_json::Value::Null))))
        })
        .await
        .ok()
        .flatten()?;
    let fleet = reference_revision(store, revisions).await;
    Some(host_summary(&host, &facts, fleet.as_deref()))
}
