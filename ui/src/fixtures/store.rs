//! A mutable, in-memory stand-in for the server.
//!
//! Demo mode is only useful if it behaves like the application: approving a
//! phase, pausing a host or editing labels has to stick, or the pages that
//! exist to do those things cannot be reviewed. The fixtures are loaded into
//! this store, which the demo branches of [`crate::api`] read and write and
//! which lives as long as the tab. It forgets everything on reload, so every
//! demo starts from the same place.

use std::cell::RefCell;

use buckle_api::{
    AgentUpgrade, ApproveRequest, AuditEntry, Connection, ContinueRequest, Fleet, HostDetail,
    HostPatch, HostPreview, HostRolloutState, HostStatus, HostSummary, LiveEvent, Me, Page,
    PhaseState, RevisionStatus, RevisionSummary, RolloutDetail, RolloutEvent, RolloutState,
    RolloutSummary, RunReport, RunSummary, UpgradeState,
};
use buckle_api::{
    JoinMethod, JoinOptions, JoinPlatform, JoinRequest, JoinState, JoinTicket, TailscaleJoinOptions,
};
use chrono::Utc;

use super::{data, flag};
use crate::api::ApiError;
use crate::logic::live;

pub(super) struct State {
    pub hosts: Vec<HostSummary>,
    /// Cluster targets, listed beside the hosts.
    pub clusters: Vec<HostSummary>,
    pub details: Vec<HostDetail>,
    pub rollouts: Vec<RolloutDetail>,
    pub revisions: Vec<RevisionSummary>,
    pub audit: Vec<AuditEntry>,
    /// Events the demo produced, for the live provider to deliver.
    pub outbox: Vec<LiveEvent>,
    /// How far `?demo&live` has moved pi-4's run on.
    pub run_frame: usize,
    /// Join commands made with Add an agent.
    pub joins: Vec<JoinTicket>,
}

impl State {
    fn new() -> Self {
        let now = Utc::now();
        let empty = flag("empty").is_some();
        let hosts = if empty { vec![] } else { data::hosts(now) };
        let clusters = if empty { vec![] } else { data::clusters(now) };
        let details = hosts
            .iter()
            .chain(clusters.iter())
            .map(|h| data::host_detail(h, now))
            .collect();
        let mut rollouts = if empty { vec![] } else { data::rollouts(now) };
        if flag("awaiting").is_some()
            && let Some(active) = rollouts.iter_mut().find(|r| r.summary.id == "42")
        {
            data::hold_at_cloud_gate(active, now);
        }
        Self {
            hosts,
            clusters,
            details,
            rollouts,
            revisions: if empty { vec![] } else { data::revisions(now) },
            audit: if empty { vec![] } else { data::audit(now) },
            outbox: Vec::new(),
            run_frame: 0,
            joins: Vec::new(),
        }
    }

    fn record(&mut self, actor: &str, action: &str, subject: &str, message: String) {
        let id = self.audit.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        let entry = AuditEntry {
            id,
            at: Utc::now(),
            actor: actor.into(),
            action: action.into(),
            outcome: "success".into(),
            subject: Some(subject.into()),
            message: message.clone(),
            detail: None,
            trace_id: None,
        };
        self.outbox.push(LiveEvent::Audit {
            entry: buckle_api::AuditEntryRef {
                id,
                at: entry.at,
                action: entry.action.clone(),
                message,
            },
        });
        self.audit.insert(0, entry);
    }

    fn host_changed(&mut self, id: &str) -> Option<HostSummary> {
        let host = self.hosts.iter().find(|h| h.id == id)?.clone();
        if let Some(d) = self.details.iter_mut().find(|d| d.summary.id == id) {
            d.summary = host.clone();
        }
        self.outbox
            .push(LiveEvent::HostChanged { host: host.clone() });
        Some(host)
    }

    pub(super) fn rollout_mut(&mut self, id: &str) -> Result<&mut RolloutDetail, ApiError> {
        self.rollouts
            .iter_mut()
            .find(|r| r.summary.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No rollout '{id}'.")))
    }
}

thread_local! {
    pub(super) static STATE: RefCell<State> = RefCell::new(State::new());
}

pub(super) fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// The events produced since the last call.
pub fn drain() -> Vec<LiveEvent> {
    with(|s| std::mem::take(&mut s.outbox))
}

fn actor() -> String {
    data::me().subject
}

/// Refuses an administrative action for `?demo&role=viewer`, the way a server
/// whose `admin_acl` does not match would.
fn admin() -> Result<(), ApiError> {
    if flag("role").as_deref() == Some("viewer") {
        Err(ApiError::Forbidden(
            "You are not allowed to change the fleet: admin_acl did not match.".into(),
        ))
    } else {
        Ok(())
    }
}

pub fn me() -> Result<Option<Me>, ApiError> {
    let viewer = flag("role").as_deref() == Some("viewer");
    match flag("auth").as_deref() {
        Some("expired") => Err(ApiError::Unauthorized),
        // A server without OIDC; with `role=viewer`, one whose admin_acl does
        // not match the request.
        Some("off") => Ok(Some(Me::anonymous(!viewer))),
        _ => {
            let mut me = data::me();
            if viewer {
                me = Me {
                    name: "Victor Viewer".into(),
                    email: Some("victor@example.com".into()),
                    subject: "victor".into(),
                    is_admin: false,
                    anonymous: false,
                };
            }
            Ok(Some(me))
        }
    }
}

pub fn refresh() -> Result<(), ApiError> {
    if flag("session").as_deref() == Some("ended") {
        return Err(ApiError::Unauthorized);
    }
    match flag("auth").as_deref() {
        Some("expired") => Err(ApiError::Unauthorized),
        _ => Ok(()),
    }
}

pub fn fleet() -> Result<Fleet, ApiError> {
    with(|s| {
        let active = s
            .rollouts
            .iter()
            .find(|r| crate::logic::tone::rollout_active(r.summary.state))
            .map(|r| r.summary.id.clone());
        let mut counts = live::recount(&s.hosts);
        counts.clusters = s.clusters.len() as u32;
        Ok(Fleet {
            counts,
            hosts: s.hosts.clone(),
            clusters: s.clusters.clone(),
            stable_revision: s
                .revisions
                .iter()
                .find(|r| r.is_stable)
                .map(|r| r.id.clone()),
            active_rollout: active,
            server_version: Some(data::SERVER_VERSION.into()),
            desired_agent_version: Some(data::SERVER_VERSION.into()),
        })
    })
}

pub fn host(id: &str) -> Result<HostDetail, ApiError> {
    with(|s| {
        s.details
            .iter()
            .find(|d| d.summary.id == id)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("No host '{id}'.")))
    })
}

pub fn host_runs(id: &str) -> Result<Vec<RunSummary>, ApiError> {
    if flag("runs").as_deref() == Some("fail") {
        return Err(ApiError::Server("The runs could not be read.".into()));
    }
    host(id).map(|d| d.recent_runs)
}

/// The latest run as it stands (the live demo moves it on), or an older one
/// built from its summary.
pub fn host_run(id: &str, run: &str) -> Result<RunReport, ApiError> {
    let detail = host(id)?;
    if let Some(latest) = detail.latest_run.filter(|r| r.id == run) {
        return Ok(latest);
    }
    detail
        .recent_runs
        .iter()
        .find(|r| r.id == run)
        .map(|r| super::runs::report_for(id, r, Utc::now()))
        .ok_or_else(|| ApiError::NotFound(format!("No run '{run}' on '{id}'.")))
}

pub fn rollout_host_report(id: &str, host: &str) -> Result<RunReport, ApiError> {
    let run = with(|s| {
        s.rollout_mut(id).map(|r| {
            r.phases
                .iter()
                .flat_map(|p| &p.hosts)
                .find(|h| crate::logic::units::key(h) == host)
                .and_then(|h| h.run_id.clone())
        })
    })?
    .ok_or_else(|| ApiError::NotFound(format!("{host} has no run in rollout {id}.")))?;
    host_run(buckle_api::split_placement_key(host).0, &run)
}

pub fn patch_host(id: &str, patch: &HostPatch) -> Result<HostSummary, ApiError> {
    admin()?;
    with(|s| {
        let host = s
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No host '{id}'.")))?;
        if let Some(labels) = &patch.labels {
            host.labels = labels.clone();
            host.effective_labels = crate::logic::labels::effective(&host.agent_labels, labels);
            host.groups = data::groups_for(&host.effective_labels);
        }
        if let Some(notes) = &patch.notes {
            host.notes = notes.clone();
        }
        s.record(
            &actor(),
            "host.update",
            id,
            format!("{} updated {id}", actor()),
        );
        Ok(s.host_changed(id).expect("the host exists"))
    })
}

pub fn move_steward(id: &str, host: &str, note: &str) -> Result<HostSummary, ApiError> {
    admin()?;
    with(|s| {
        let cluster = s
            .clusters
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No target '{id}'.")))?;
        let status = cluster.cluster_status.get_or_insert_with(Default::default);
        status.steward = Some(host.to_owned());
        status.executor = Some(host.to_owned());
        let cluster = cluster.clone();
        if let Some(d) = s.details.iter_mut().find(|d| d.summary.id == id) {
            d.summary = cluster.clone();
        }
        let why = if note.is_empty() {
            String::new()
        } else {
            format!(": {note}")
        };
        s.record(
            &actor(),
            "target.steward",
            id,
            format!("{} moved {id}'s lease to {host}{why}", actor()),
        );
        s.outbox.push(LiveEvent::HostChanged {
            host: cluster.clone(),
        });
        Ok(cluster)
    })
}

pub fn host_action(id: &str, action: &str) -> Result<HostSummary, ApiError> {
    admin()?;
    with(|s| {
        let host = s
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No host '{id}'.")))?;
        host.status = match (action, host.status) {
            ("approve", HostStatus::Pending) => HostStatus::Active,
            ("pause", HostStatus::Active | HostStatus::Pending) => HostStatus::Paused,
            ("resume", HostStatus::Paused) => HostStatus::Active,
            (_, status) => {
                return Err(ApiError::Conflict(format!(
                    "Host '{id}' is {status:?}; it cannot be {action}d."
                )));
            }
        };
        s.record(
            &actor(),
            &format!("host.{action}"),
            id,
            format!("{} {action}d {id}", actor()),
        );
        Ok(s.host_changed(id).expect("the host exists"))
    })
}

pub fn run_action(
    id: &str,
    module: &str,
    action: &str,
    note: &str,
) -> Result<buckle_api::ActionStarted, ApiError> {
    admin()?;
    with(|s| {
        let detail = s
            .details
            .iter()
            .find(|d| d.summary.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No target '{id}'.")))?;
        if detail.summary.connection == Connection::Offline {
            return Err(ApiError::Conflict(format!("{id} is offline.")));
        }
        let info = detail
            .actions
            .iter()
            .find(|a| a.module == module && a.name == action)
            .ok_or_else(|| {
                ApiError::NotFound(format!("{id} offers no action '{action}' of '{module}'."))
            })?
            .clone();
        let noun = match detail.summary.kind {
            buckle_api::TargetKind::Host => "host",
            buckle_api::TargetKind::Cluster | buckle_api::TargetKind::Workload => "target",
        };
        let ran = format!("{} ran {module}/{action} on {id}", actor());
        s.record(
            &actor(),
            &format!("{noun}.action.run"),
            id,
            if note.is_empty() {
                ran
            } else {
                format!("{ran}: {note}")
            },
        );
        Ok(buckle_api::ActionStarted {
            run_id: format!("demo-action-{}-{}", info.module, info.name),
        })
    })
}

/// The demo answers a drift check at once: the target reads as checked now.
pub fn check_drift(id: &str, workload: Option<&str>) -> Result<HostSummary, ApiError> {
    admin()?;
    with(|s| {
        let now = Utc::now();
        let scope = match workload {
            Some(w) => format!("{w} on {id}"),
            None => id.to_owned(),
        };
        if let Some(host) = s.hosts.iter_mut().find(|h| h.id == id) {
            if host.connection == Connection::Offline {
                return Err(ApiError::Conflict(format!("{id} is offline.")));
            }
            host.drift_at = Some(now);
            s.record(
                &actor(),
                "host.drift.check",
                id,
                format!("{} asked {scope} to check for drift", actor()),
            );
            return Ok(s.host_changed(id).expect("the host exists"));
        }
        let cluster = s
            .clusters
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No target '{id}'.")))?;
        cluster.drift_at = Some(now);
        let cluster = cluster.clone();
        if let Some(d) = s.details.iter_mut().find(|d| d.summary.id == id) {
            d.summary = cluster.clone();
        }
        s.record(
            &actor(),
            "target.drift.check",
            id,
            format!("{} asked {scope} to check for drift", actor()),
        );
        s.outbox.push(LiveEvent::HostChanged {
            host: cluster.clone(),
        });
        Ok(cluster)
    })
}

pub fn upgrade_host(id: &str, version: Option<&str>) -> Result<HostSummary, ApiError> {
    admin()?;
    with(|s| {
        let host = s
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No host '{id}'.")))?;
        if host.connection == Connection::Offline {
            return Err(ApiError::Conflict(format!(
                "{id} is offline; its agent can be upgraded once it reconnects."
            )));
        }
        let version = version
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(data::SERVER_VERSION)
            .trim()
            .trim_start_matches('v')
            .to_owned();
        if host.agent_version.as_deref() == Some(version.as_str()) {
            return Err(ApiError::Conflict(format!(
                "{id}'s agent already runs {version}."
            )));
        }
        let from = host.agent_version.clone().unwrap_or_default();
        host.upgrade = Some(AgentUpgrade {
            version: version.clone(),
            state: UpgradeState::Requested,
            requested_at: Utc::now(),
            requested_by: actor(),
            error: None,
        });
        s.record(
            &actor(),
            "host.upgrade",
            id,
            format!(
                "{} asked {id} to upgrade its agent from {from} to {version}",
                actor()
            ),
        );
        Ok(s.host_changed(id).expect("the host exists"))
    })
}

pub fn forget_host(id: &str) -> Result<(), ApiError> {
    admin()?;
    with(|s| {
        let before = s.hosts.len();
        s.hosts.retain(|h| h.id != id);
        s.details.retain(|d| d.summary.id != id);
        if s.hosts.len() == before {
            return Err(ApiError::NotFound(format!("No host '{id}'.")));
        }
        s.record(
            &actor(),
            "host.forget",
            id,
            format!("{} forgot {id}", actor()),
        );
        Ok(())
    })
}

pub fn preview_host(id: &str, patch: &HostPatch) -> Result<HostPreview, ApiError> {
    with(|s| {
        let host = s
            .hosts
            .iter()
            .find(|h| h.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("No host '{id}'.")))?;
        let labels = patch.labels.clone().unwrap_or_else(|| host.labels.clone());
        let effective = crate::logic::labels::effective(&host.agent_labels, &labels);
        let groups = data::groups_for(&effective);
        let phase = data::phase_for(&groups);
        Ok(HostPreview {
            effective_labels: effective,
            groups,
            phase,
        })
    })
}

fn page<T: Clone>(items: &[T], offset: u64, limit: usize) -> Page<T> {
    let start = (offset as usize).min(items.len());
    let end = (start + limit).min(items.len());
    Page {
        items: items[start..end].to_vec(),
        next: (end < items.len()).then(|| end.to_string()),
        total: items.len() as u64,
    }
}

pub fn revisions(offset: u64) -> Result<Page<RevisionSummary>, ApiError> {
    with(|s| Ok(page(&s.revisions, offset, 50)))
}

/// The demo has no validator: a revision keeps its status.
pub fn revalidate(revision: &str) -> Result<RevisionSummary, ApiError> {
    admin()?;
    with(|s| {
        s.revisions
            .iter()
            .find(|r| r.id == revision)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("No revision '{revision}'.")))
    })
}

pub fn start_rollout(revision: &str, note: &str) -> Result<RolloutSummary, ApiError> {
    admin()?;
    with(|s| {
        // supersede: latest. A rollout started while another runs waits for
        // it (`detected`), and skips any rollout already waiting.
        let busy = s
            .rollouts
            .iter()
            .any(|r| crate::logic::tone::rollout_active(r.summary.state));
        let rev = s
            .revisions
            .iter()
            .find(|r| r.id == revision)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("No revision '{revision}'.")))?;
        if rev.status == RevisionStatus::Rejected {
            return Err(ApiError::Conflict(
                "A rejected revision cannot be rolled out.".into(),
            ));
        }
        let id = (s
            .rollouts
            .iter()
            .filter_map(|r| r.summary.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1)
        .to_string();
        let mut detail = data::active_rollout(Utc::now());
        detail.summary.id = id.clone();
        detail.summary.revision = rev.id.clone();
        detail.summary.revision_message = rev.message.clone();
        detail.summary.state = if busy {
            RolloutState::Detected
        } else {
            RolloutState::Planning
        };
        detail.summary.triggered_by = actor();
        detail.summary.started_at = Utc::now();
        detail.summary.current_phase = None;
        detail.events.clear();
        for phase in &mut detail.phases {
            phase.state = PhaseState::Pending;
            phase.approved_by = None;
            for h in &mut phase.hosts {
                h.state = HostRolloutState::PlanPending;
                h.error = None;
                h.failed_checks.clear();
            }
        }
        if let Some(r) = s.revisions.iter_mut().find(|r| r.id == revision) {
            r.rollout_id = Some(id.clone());
        }
        for r in &mut s.rollouts {
            if r.summary.state == RolloutState::Detected {
                r.summary.state = RolloutState::Skipped;
                r.summary.finished_at = Some(Utc::now());
                r.summary.end_reason = Some(format!("superseded by {id}"));
                r.summary.superseded_by = Some(id.clone());
                s.outbox.push(LiveEvent::RolloutChanged {
                    rollout: r.summary.clone(),
                });
            }
        }
        let summary = detail.summary.clone();
        s.rollouts.insert(0, detail);
        s.record(
            &actor(),
            "rollout.start",
            &id,
            format!(
                "{} started rollout {id} of {}. {note}",
                actor(),
                &revision[..7.min(revision.len())]
            ),
        );
        s.outbox.push(LiveEvent::RolloutChanged {
            rollout: summary.clone(),
        });
        Ok(summary)
    })
}

/// The demo reads a selector loosely: a target matches when its name or
/// one of its labels' values is quoted in it.
fn demo_selects(selector: &str, target: &HostSummary) -> bool {
    let selector = selector.trim();
    selector.is_empty()
        || selector.contains(&format!("\"{}\"", target.id))
        || target
            .effective_labels
            .values()
            .any(|v| selector.contains(&format!("\"{v}\"")))
}

pub fn fleet_actions(selector: &str) -> Result<Vec<buckle_api::FleetAction>, ApiError> {
    with(|s| {
        let mut out: Vec<buckle_api::FleetAction> = Vec::new();
        for d in &s.details {
            if d.summary.status != HostStatus::Active || !demo_selects(selector, &d.summary) {
                continue;
            }
            let workload = d.summary.kind != buckle_api::TargetKind::Host;
            for a in &d.actions {
                match out.iter_mut().find(|f| {
                    f.workload == workload && f.action.module == a.module && f.action.name == a.name
                }) {
                    Some(f) => f.targets.push(d.summary.id.clone()),
                    None => out.push(buckle_api::FleetAction {
                        action: a.clone(),
                        workload,
                        targets: vec![d.summary.id.clone()],
                    }),
                }
            }
        }
        out.sort_by(|a, b| {
            (a.workload, &a.action.module, &a.action.name).cmp(&(
                b.workload,
                &b.action.module,
                &b.action.name,
            ))
        });
        Ok(out)
    })
}

pub fn start_action_rollout(
    body: &buckle_api::StartActionRollout,
) -> Result<RolloutSummary, ApiError> {
    admin()?;
    let selector = body.selector.clone().unwrap_or_default();
    let found = fleet_actions(&selector)?
        .into_iter()
        .find(|f| {
            f.workload == body.workload
                && f.action.module == body.module
                && f.action.name == body.action
        })
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "No target offers the action '{}' of '{}'.",
                body.action, body.module
            ))
        })?;
    // The demo always has a rollout under way, which the server would refuse
    // to start beside: the demo starts it anyway.
    with(|s| {
        let stable = s
            .revisions
            .iter()
            .find(|r| r.is_stable)
            .cloned()
            .ok_or_else(|| ApiError::Conflict("There is no stable revision yet.".into()))?;
        let id = (s
            .rollouts
            .iter()
            .filter_map(|r| r.summary.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1)
        .to_string();
        let mut detail = data::active_rollout(Utc::now());
        detail.summary.id = id.clone();
        detail.summary.revision = stable.id.clone();
        detail.summary.revision_message =
            format!("{} ({}/{})", found.action.label, body.module, body.action);
        detail.summary.action = Some(buckle_api::RolloutActionInfo {
            module: body.module.clone(),
            action: body.action.clone(),
            label: found.action.label.clone(),
            selector: body.selector.clone(),
        });
        detail.summary.state = RolloutState::Rolling;
        detail.summary.triggered_by = actor();
        detail.summary.started_at = Utc::now();
        detail.events.clear();
        detail.plans.clear();
        for (i, phase) in detail.phases.iter_mut().enumerate() {
            phase.state = if i == 0 {
                PhaseState::Open
            } else {
                PhaseState::Pending
            };
            phase.approved_by = None;
            phase.hosts.retain(|h| found.targets.contains(&h.host));
            if body.workload {
                // One placement of the workload per cluster, as the server freezes them.
                let mut seen = std::collections::BTreeSet::new();
                phase.hosts.retain(|h| seen.insert(h.host.clone()));
                for h in &mut phase.hosts {
                    h.workload = Some(body.module.clone());
                    h.kind = buckle_api::TargetKind::Workload;
                }
            }
            for h in &mut phase.hosts {
                h.state = HostRolloutState::Planned;
                h.error = None;
                h.failed_checks.clear();
            }
        }
        detail.summary.current_phase = detail.phases.first().map(|p| p.name.clone());
        detail.summary.hosts_total = detail.phases.iter().map(|p| p.hosts.len() as u32).sum();
        detail.summary.hosts_done = 0;
        detail.summary.hosts_failed = 0;
        let summary = detail.summary.clone();
        s.rollouts.insert(0, detail);
        s.record(
            &actor(),
            "rollout.detected",
            &id,
            format!(
                "rollout of {}/{} on {} targets created by {}. {}",
                body.module,
                body.action,
                found.targets.len(),
                actor(),
                body.note
            ),
        );
        s.outbox.push(LiveEvent::RolloutChanged {
            rollout: summary.clone(),
        });
        Ok(summary)
    })
}

pub fn rollouts(offset: u64) -> Result<Page<RolloutSummary>, ApiError> {
    with(|s| {
        let list: Vec<RolloutSummary> = s.rollouts.iter().map(with_changes).collect();
        Ok(page(&list, offset, 50))
    })
}

pub fn rollout(id: &str) -> Result<RolloutDetail, ApiError> {
    with(|s| {
        s.rollout_mut(id).map(|r| RolloutDetail {
            summary: with_changes(r),
            ..r.clone()
        })
    })
}

/// A rollout's summary with what it changes, as the server fills it in.
fn with_changes(d: &RolloutDetail) -> RolloutSummary {
    RolloutSummary {
        changes: d
            .fleet
            .as_ref()
            .filter(|f| !f.hosts.is_empty())
            .map(|f| f.changes()),
        ..d.summary.clone()
    }
}

fn event(kind: &str, message: String, host: Option<&str>, phase: Option<&str>) -> RolloutEvent {
    RolloutEvent {
        at: Utc::now(),
        kind: kind.into(),
        message,
        // A placement's `cluster/workload` key names both, as the server does.
        host: host.map(|h| buckle_api::split_placement_key(h).0.into()),
        workload: host.and_then(|h| buckle_api::split_placement_key(h).1.map(Into::into)),
        phase: phase.map(Into::into),
        actor: Some(actor()),
    }
}

/// The timeline entries a demo action added (from index `before`), as the
/// server's stream would deliver them.
pub(super) fn timeline(detail: &RolloutDetail, before: usize) -> Vec<LiveEvent> {
    detail.events[before.min(detail.events.len())..]
        .iter()
        .map(|event| LiveEvent::RolloutTimeline {
            rollout_id: detail.summary.id.clone(),
            event: event.clone(),
        })
        .collect()
}

pub fn approve(id: &str, request: &ApproveRequest) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        let detail = s.rollout_mut(id)?;
        let before = detail.events.len();
        let index = detail
            .phases
            .iter()
            .position(|p| p.name == request.phase)
            .ok_or_else(|| ApiError::NotFound(format!("No phase '{}'.", request.phase)))?;
        // As the server: a phase is approved once the phases before it finish.
        let running: Vec<&str> = detail.phases[..index]
            .iter()
            .filter(|p| !matches!(p.state, PhaseState::Done | PhaseState::Skipped))
            .map(|p| p.name.as_str())
            .collect();
        if !running.is_empty() {
            return Err(ApiError::Conflict(format!(
                "Phase '{}' can be approved once {} finished.",
                request.phase,
                running.join(", ")
            )));
        }
        let phase = &mut detail.phases[index];
        if phase.approved_by.is_some() && phase.hosts.iter().all(|h| !h.needs_approval) {
            return Err(ApiError::Conflict(format!(
                "Phase {} is already approved.",
                phase.name
            )));
        }
        let mut covered = 0;
        for h in &mut phase.hosts {
            if h.plan_digest
                .as_ref()
                .is_some_and(|d| request.plan_digests.contains(d))
            {
                h.needs_approval = false;
                covered += 1;
            }
        }
        phase.approved_by = Some(actor());
        phase.approved_at = Some(Utc::now());
        if phase.state == PhaseState::AwaitingApproval {
            phase.state = PhaseState::Pending;
        }
        let name = phase.name.clone();
        let changed: Vec<_> = phase.hosts.clone();
        let message = format!(
            "Approved {} for {covered} hosts.",
            crate::logic::format::count(request.plan_digests.len(), "plan", "plans")
        );
        detail
            .events
            .push(event("phase.approved", message.clone(), None, Some(&name)));
        let summary = detail.summary.clone();
        let snapshot = detail.clone();
        s.outbox.extend(timeline(&snapshot, before));
        for h in changed {
            s.outbox.push(LiveEvent::RolloutHostChanged {
                rollout_id: id.into(),
                host: h,
            });
        }
        s.outbox
            .push(LiveEvent::RolloutChanged { rollout: summary });
        s.record(
            &actor(),
            "rollout.approve",
            id,
            format!(
                "{} approved {name} of rollout {id}: {message} {}",
                actor(),
                request.note
            ),
        );
        Ok(snapshot)
    })
}

pub fn rollout_action(id: &str, action: &str, note: &str) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        let detail = s.rollout_mut(id)?;
        let before = detail.events.len();
        let state = detail.summary.state;
        // Host-wide decisions: they act on the rollout's failed or baking
        // hosts, and a retry lifts a pause.
        let hosts = |want: HostRolloutState| {
            detail
                .phases
                .iter()
                .flat_map(|p| &p.hosts)
                .filter(|h| h.state == want)
                .count()
        };
        let next = match (action, state) {
            ("retry", s)
                if !crate::logic::tone::rollout_finished(s)
                    && hosts(HostRolloutState::Failed) > 0 =>
            {
                detail
                    .phases
                    .iter_mut()
                    .flat_map(|p| p.hosts.iter_mut())
                    .filter(|h| h.state == HostRolloutState::Failed)
                    .for_each(replan);
                if s == RolloutState::Paused {
                    RolloutState::Rolling
                } else {
                    s
                }
            }
            ("expedite", s)
                if !crate::logic::tone::rollout_finished(s)
                    && hosts(HostRolloutState::Baking) > 0 =>
            {
                detail
                    .phases
                    .iter_mut()
                    .flat_map(|p| p.hosts.iter_mut())
                    .filter(|h| h.state == HostRolloutState::Baking)
                    .for_each(expedite);
                s
            }
            (
                "pause",
                RolloutState::Rolling
                | RolloutState::Baking
                | RolloutState::AwaitingApproval
                | RolloutState::Planning,
            ) => RolloutState::Paused,
            ("resume", RolloutState::Paused) => RolloutState::Rolling,
            ("rollback", RolloutState::Paused | RolloutState::Rolling | RolloutState::Baking) => {
                RolloutState::RolledBack
            }
            ("abort", s) if !crate::logic::tone::rollout_finished(s) => RolloutState::Aborted,
            _ => {
                return Err(ApiError::Conflict(format!(
                    "Rollout {id} is {}; it cannot {action}.",
                    crate::logic::tone::rollout_state_label(state)
                )));
            }
        };
        detail.summary.state = next;
        if next == RolloutState::Paused {
            detail.summary.pause_reason = Some(format!("Paused by {}. {note}", actor()));
        } else if next == RolloutState::Rolling {
            detail.summary.pause_reason = None;
        }
        if crate::logic::tone::rollout_finished(next) {
            detail.summary.finished_at = Some(Utc::now());
            detail.summary.current_phase = None;
        }
        if next == RolloutState::RolledBack {
            for phase in &mut detail.phases {
                for h in &mut phase.hosts {
                    if matches!(
                        h.state,
                        HostRolloutState::Healthy
                            | HostRolloutState::Baking
                            | HostRolloutState::Applying
                            | HostRolloutState::Failed
                    ) {
                        h.state = HostRolloutState::RolledBack;
                    }
                }
            }
        }
        detail.events.push(event(
            &format!("rollout.{action}"),
            format!("{action} by {}. {note}", actor()),
            None,
            None,
        ));
        let snapshot = detail.clone();
        s.outbox.extend(timeline(&snapshot, before));
        s.outbox.push(LiveEvent::RolloutChanged {
            rollout: snapshot.summary.clone(),
        });
        s.record(
            &actor(),
            &format!("rollout.{action}"),
            id,
            format!("{} {action} rollout {id}. {note}", actor()),
        );
        Ok(snapshot)
    })
}

pub fn reject(id: &str, reason: &str) -> Result<RolloutDetail, ApiError> {
    admin()?;
    if reason.trim().is_empty() {
        return Err(ApiError::Server(
            "Say why the rollout is rejected: the reason is recorded in the audit log.".into(),
        ));
    }
    with(|s| {
        let detail = s.rollout_mut(id)?;
        let before = detail.events.len();
        if detail.summary.state != RolloutState::AwaitingApproval {
            return Err(ApiError::Conflict(format!(
                "Rollout {id} is not awaiting approval, so there is nothing to reject; abort it instead."
            )));
        }
        let phase = detail.summary.current_phase.clone();
        detail.summary.state = RolloutState::Aborted;
        detail.summary.finished_at = Some(Utc::now());
        detail.summary.current_phase = None;
        detail.events.push(event(
            "rollout.abort",
            format!("{} aborted the rollout. Rejected: {reason}", actor()),
            None,
            None,
        ));
        detail.events.push(event(
            "rollout.reject",
            format!("{} rejected the rollout: {reason}", actor()),
            None,
            phase.as_deref(),
        ));
        let snapshot = detail.clone();
        s.outbox.extend(timeline(&snapshot, before));
        s.outbox.push(LiveEvent::RolloutChanged {
            rollout: snapshot.summary.clone(),
        });
        s.record(
            &actor(),
            "rollout.reject",
            id,
            format!("{} rejected the rollout: {reason}", actor()),
        );
        Ok(snapshot)
    })
}

fn host_entry(
    s: &mut State,
    id: &str,
    host: &str,
    f: impl FnOnce(&mut buckle_api::RolloutHost) -> Result<String, ApiError>,
) -> Result<RolloutDetail, ApiError> {
    let detail = s.rollout_mut(id)?;
    let before = detail.events.len();
    let entry = detail
        .phases
        .iter_mut()
        .flat_map(|p| p.hosts.iter_mut())
        .find(|h| crate::logic::units::key(h) == host)
        .ok_or_else(|| ApiError::NotFound(format!("Host '{host}' is not in rollout {id}.")))?;
    let message = f(entry)?;
    entry.updated_at = Utc::now();
    let (changed, phase) = (entry.clone(), entry.phase.clone());
    detail.events.push(event(
        "host.operator",
        message.clone(),
        Some(host),
        Some(&phase),
    ));
    let snapshot = detail.clone();
    s.outbox.push(LiveEvent::RolloutHostChanged {
        rollout_id: id.into(),
        host: changed,
    });
    s.outbox.extend(timeline(&snapshot, before));
    s.record(&actor(), "rollout.host", host, message);
    Ok(snapshot)
}

/// A host re-planned (retried): its plan and error are gone until it answers.
fn replan(h: &mut buckle_api::RolloutHost) {
    h.state = HostRolloutState::PlanPending;
    h.error = None;
    h.error_detail = None;
    h.failed_checks.clear();
}

fn expedite(h: &mut buckle_api::RolloutHost) {
    h.state = HostRolloutState::Healthy;
    h.bake_until = None;
}

pub fn rollout_phase_action(
    id: &str,
    phase: &str,
    action: &str,
    note: &str,
) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        let detail = s.rollout_mut(id)?;
        let before = detail.events.len();
        let p = detail
            .phases
            .iter_mut()
            .find(|p| p.name == phase)
            .ok_or_else(|| ApiError::NotFound(format!("Rollout {id} has no phase '{phase}'.")))?;
        if action != "expedite" {
            return Err(ApiError::NotFound(format!("No phase action '{action}'.")));
        }
        let baking: Vec<&mut buckle_api::RolloutHost> = p
            .hosts
            .iter_mut()
            .filter(|h| h.state == HostRolloutState::Baking)
            .collect();
        if baking.is_empty() {
            return Err(ApiError::Conflict(format!(
                "No host in {phase} is baking, so there is nothing to expedite."
            )));
        }
        let mut changed = Vec::new();
        for h in baking {
            expedite(h);
            h.updated_at = Utc::now();
            changed.push(h.clone());
        }
        p.bake_until = None;
        let message = format!("{} expedited {phase}. {note}", actor());
        detail.events.push(event(
            "rollout.operator",
            message.clone(),
            None,
            Some(phase),
        ));
        let snapshot = detail.clone();
        s.outbox.extend(
            changed
                .into_iter()
                .map(|host| LiveEvent::RolloutHostChanged {
                    rollout_id: id.into(),
                    host,
                }),
        );
        s.outbox.extend(timeline(&snapshot, before));
        s.record(&actor(), "rollout.phase", phase, message);
        Ok(snapshot)
    })
}

pub fn rollout_host_action(
    id: &str,
    host: &str,
    action: &str,
    note: &str,
) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        host_entry(s, id, host, |h| {
            match (action, h.state) {
                (
                    "retry",
                    HostRolloutState::PlanPending
                    | HostRolloutState::Planned
                    | HostRolloutState::Failed
                    | HostRolloutState::Held
                    | HostRolloutState::Skipped
                    | HostRolloutState::Pending,
                ) => replan(h),
                ("expedite", HostRolloutState::Baking) => expedite(h),
                ("skip", _) => {
                    h.state = HostRolloutState::Skipped;
                    h.error = Some(format!("Skipped by {}: {note}", actor()));
                }
                (
                    "force",
                    HostRolloutState::Held
                    | HostRolloutState::Planned
                    | HostRolloutState::Failed
                    | HostRolloutState::Healthy
                    | HostRolloutState::Skipped
                    | HostRolloutState::Pending,
                ) => {
                    h.state = HostRolloutState::Applying;
                    h.error = None;
                }
                (_, state) => {
                    return Err(ApiError::Conflict(format!(
                        "{host} is {}; it cannot be {action}ed.",
                        crate::logic::tone::host_state_label(state)
                    )));
                }
            }
            Ok(format!("{} {action} {host}. {note}", actor()))
        })
    })
}

pub fn continue_host(
    id: &str,
    host: &str,
    request: &ContinueRequest,
) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        host_entry(s, id, host, |h| {
            if h.state != HostRolloutState::Failed || h.failed_checks.is_empty() {
                return Err(ApiError::Conflict(format!(
                    "{host} has no failed health check to waive; retry it instead."
                )));
            }
            h.waived_checks = request.waive_checks.clone();
            h.failed_checks
                .retain(|c| !request.waive_checks.contains(c));
            h.state = HostRolloutState::Applying;
            h.error = None;
            Ok(format!(
                "{} continued {host} waiving {}. {}",
                actor(),
                request.waive_checks.join(", "),
                request.note
            ))
        })
    })
}

pub fn audit(
    filter: &str,
    before: Option<u64>,
    progress: bool,
) -> Result<Page<AuditEntry>, ApiError> {
    if let crate::logic::filter::Check::Invalid { message, .. } =
        crate::logic::filter::check(filter)
    {
        return Err(ApiError::Server(format!(
            "The filter is not valid: {message}"
        )));
    }
    with(|s| {
        let matched: Vec<AuditEntry> = crate::logic::filter::filter_entries(&s.audit, filter)
            .into_iter()
            .filter(|e| before.is_none_or(|b| e.id < b))
            .filter(|e| progress || !e.is_progress())
            .cloned()
            .collect();
        let total = matched.len() as u64;
        let items: Vec<AuditEntry> = matched.into_iter().take(100).collect();
        let next = ((items.len() as u64) < total)
            .then(|| items.last().map(|e| e.id.to_string()))
            .flatten();
        Ok(Page { items, next, total })
    })
}

// ------------------------------------------------------------------ joining hosts

/// A tailnet server with Tailscale provisioning that also takes tokens.
pub fn join_options() -> Result<JoinOptions, ApiError> {
    admin()?;
    Ok(JoinOptions {
        methods: vec![JoinMethod::Tailscale, JoinMethod::Token],
        server_url: "http://buckle.example.ts.net:7700".into(),
        tailscale: Some(TailscaleJoinOptions {
            tags: vec!["tag:buckle".into(), "tag:edge".into()],
            consent: true,
        }),
        ttl_seconds: 3600,
        approval_required: true,
    })
}

/// What the server would render, near enough to review the wizard by.
fn demo_command(r: &JoinRequest, tags: &[String]) -> String {
    let windows = r.platform == JoinPlatform::Windows;
    let (head, cont) = if windows {
        (
            "& ([scriptblock]::Create((irm https://github.com/SierraSoftworks/buckle/releases/latest/download/install.ps1)))",
            " `",
        )
    } else {
        (
            "curl -fsSL https://github.com/SierraSoftworks/buckle/releases/latest/download/install.sh | sudo sh -s --",
            " \\",
        )
    };
    let mut lines = vec![head.to_owned()];
    if r.method == JoinMethod::Tailscale {
        let (key, tag_flag, host_flag) = if windows {
            ("-TailscaleAuthKey", "-TailscaleTags", "-TailscaleHostname")
        } else {
            (
                "--tailscale-auth-key",
                "--tailscale-tags",
                "--tailscale-hostname",
            )
        };
        lines.push(format!("  {key} tskey-auth-kDEMO1CNTRL-0123456789abcdef"));
        let tags = if tags.is_empty() {
            "tag:buckle".to_owned()
        } else {
            tags.join(",")
        };
        lines.push(format!("  {tag_flag} {tags}"));
        if let Some(h) = &r.host_id {
            lines.push(format!("  {host_flag} {h}"));
        }
    }
    lines.push("  up --server http://buckle.example.ts.net:7700".into());
    lines.push("    --insecure-plaintext".into());
    lines.push("    --token 3f9a0c4e1b7d2a6f9c0e".into());
    if let Some(h) = &r.host_id {
        lines.push(format!("    --host-id {h}"));
    }
    for (k, v) in &r.labels {
        lines.push(format!("    --label {k}={v}"));
    }
    let last = lines.len() - 1;
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| if i == last { l } else { format!("{l}{cont}") })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn create_join(r: &JoinRequest) -> Result<JoinTicket, ApiError> {
    admin()?;
    let now = Utc::now();
    let platform = serde_json::to_value(r.platform)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let id = format!("demo-{platform}-{}", now.timestamp_millis());
    let ticket = JoinTicket {
        id: id.clone(),
        platform: r.platform,
        method: r.method,
        host_id: r.host_id.clone(),
        labels: r.labels.clone(),
        created_by: actor(),
        created_at: now,
        expires_at: now + chrono::Duration::hours(1),
        state: match r.method {
            // Consent is skipped: the demo comes straight back with a key.
            JoinMethod::Tailscale => JoinState::Authorize {
                authorize_url: format!("/hosts?join={id}"),
            },
            JoinMethod::Token => JoinState::Ready {
                command: demo_command(r, &[]),
            },
        },
    };
    with(|s| {
        let mut stored = ticket.clone();
        if r.method == JoinMethod::Tailscale {
            stored.state = JoinState::Ready {
                command: demo_command(r, &r.tailscale_tags),
            };
        }
        s.joins.push(stored);
        s.record(
            &actor(),
            "join.created",
            &ticket.id,
            format!("{} created a join command", actor()),
        );
    });
    Ok(ticket)
}

pub fn join_ticket(id: &str) -> Result<JoinTicket, ApiError> {
    admin()?;
    with(|s| s.joins.iter().find(|t| t.id == id).cloned()).ok_or_else(|| {
        ApiError::NotFound("That join command does not exist or has expired.".into())
    })
}
