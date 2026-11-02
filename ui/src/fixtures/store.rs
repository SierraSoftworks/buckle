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
    ApproveRequest, AuditEntry, Catalog, ContinueRequest, Fleet, HostDetail, HostPatch,
    HostPreview, HostRolloutState, HostStatus, HostSummary, LiveEvent, Me, Page, PhaseState,
    RevisionStatus, RevisionSummary, RolloutDetail, RolloutEvent, RolloutState, RolloutSummary,
};
use chrono::Utc;

use super::{data, flag};
use crate::api::ApiError;
use crate::logic::live;

pub(super) struct State {
    pub hosts: Vec<HostSummary>,
    pub details: Vec<HostDetail>,
    pub rollouts: Vec<RolloutDetail>,
    pub revisions: Vec<RevisionSummary>,
    pub audit: Vec<AuditEntry>,
    /// Events the demo produced, for the live provider to deliver.
    pub outbox: Vec<LiveEvent>,
}

impl State {
    fn new() -> Self {
        let now = Utc::now();
        let empty = flag("empty").is_some();
        let hosts = if empty { vec![] } else { data::hosts(now) };
        let details = hosts.iter().map(|h| data::host_detail(h, now)).collect();
        Self {
            hosts,
            details,
            rollouts: if empty { vec![] } else { data::rollouts(now) },
            revisions: if empty { vec![] } else { data::revisions(now) },
            audit: if empty { vec![] } else { data::audit(now) },
            outbox: Vec::new(),
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
            .find(|r| !crate::logic::tone::rollout_finished(r.summary.state))
            .map(|r| r.summary.id.clone());
        Ok(Fleet {
            counts: live::recount(&s.hosts),
            hosts: s.hosts.clone(),
            stable_revision: s
                .revisions
                .iter()
                .find(|r| r.is_stable)
                .map(|r| r.id.clone()),
            active_rollout: active,
            server_version: Some(data::SERVER_VERSION.into()),
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

pub fn start_rollout(revision: &str, note: &str) -> Result<RolloutSummary, ApiError> {
    admin()?;
    with(|s| {
        if s.rollouts
            .iter()
            .any(|r| !crate::logic::tone::rollout_finished(r.summary.state))
        {
            return Err(ApiError::Conflict(
                "Rollout 42 is still running; with supersede: queue the new rollout starts when it finishes.".into(),
            ));
        }
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
        detail.summary.revision_message = crate::logic::format::first_line(&rev.message).into();
        detail.summary.state = RolloutState::Planning;
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

pub fn rollouts(offset: u64) -> Result<Page<RolloutSummary>, ApiError> {
    with(|s| {
        let list: Vec<RolloutSummary> = s.rollouts.iter().map(|r| r.summary.clone()).collect();
        Ok(page(&list, offset, 50))
    })
}

pub fn rollout(id: &str) -> Result<RolloutDetail, ApiError> {
    with(|s| s.rollout_mut(id).map(|r| r.clone()))
}

fn event(kind: &str, message: String, host: Option<&str>, phase: Option<&str>) -> RolloutEvent {
    RolloutEvent {
        at: Utc::now(),
        kind: kind.into(),
        message,
        host: host.map(Into::into),
        phase: phase.map(Into::into),
        actor: Some(actor()),
    }
}

pub fn approve(id: &str, request: &ApproveRequest) -> Result<RolloutDetail, ApiError> {
    admin()?;
    with(|s| {
        let detail = s.rollout_mut(id)?;
        let phase = detail
            .phases
            .iter_mut()
            .find(|p| p.name == request.phase)
            .ok_or_else(|| ApiError::NotFound(format!("No phase '{}'.", request.phase)))?;
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
        let state = detail.summary.state;
        let next = match (action, state) {
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

fn host_entry(
    s: &mut State,
    id: &str,
    host: &str,
    f: impl FnOnce(&mut buckle_api::RolloutHost) -> Result<String, ApiError>,
) -> Result<RolloutDetail, ApiError> {
    let detail = s.rollout_mut(id)?;
    let entry = detail
        .phases
        .iter_mut()
        .flat_map(|p| p.hosts.iter_mut())
        .find(|h| h.host == host)
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
    s.record(&actor(), "rollout.host", host, message);
    Ok(snapshot)
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
                ("retry", HostRolloutState::Failed | HostRolloutState::RolledBack) => {
                    h.state = HostRolloutState::Applying;
                    h.error = None;
                }
                ("skip", _) => {
                    h.state = HostRolloutState::Skipped;
                    h.error = Some(format!("Skipped by {}: {note}", actor()));
                }
                ("force", HostRolloutState::Held) => {
                    h.state = HostRolloutState::Applying;
                    h.error = Some(format!("Forced past a failing baseline by {}.", actor()));
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

pub fn catalog() -> Result<Catalog, ApiError> {
    Ok(data::catalog())
}

pub fn audit(filter: &str, before: Option<u64>) -> Result<Page<AuditEntry>, ApiError> {
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
