//! What needs attention (`GET /api/v1/attention`): one list, read by the UI's
//! bell and sent to webhooks, so the two cannot disagree.
//!
//! [`build`] is a pure function of what the server knows, shared with the
//! UI's demo store. Each item has a stable `id`: the same condition yields
//! the same id on every pass, so a reader can tell when it opened and when it
//! resolved.

use crate::{
    Connection, HealthSummary, HostCheck, HostRolloutState, HostStatus, HostSummary, RolloutDetail,
    RolloutState, TargetKind,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// How long an agent may be offline before it needs attention, in seconds.
pub const OFFLINE_ATTENTION: i64 = 10 * 60;

/// How long a refused identity stays on the list without a later grant.
pub const REFUSAL_ATTENTION: i64 = 24 * 60 * 60;

/// How much an item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionTone {
    Danger,
    Warn,
    Info,
}

/// What an item is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Host,
    Cluster,
    Rollout,
    Schedule,
    Webhook,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttentionSubject {
    pub kind: SubjectKind,
    pub id: String,
}

impl AttentionSubject {
    pub fn new(kind: SubjectKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    /// The subject's page in the UI, when it has one.
    pub fn path(&self) -> Option<String> {
        let id = encode(&self.id);
        match self.kind {
            SubjectKind::Host => Some(format!("/hosts/{id}")),
            SubjectKind::Cluster => Some(format!("/clusters/{id}")),
            SubjectKind::Rollout => Some(format!("/rollouts/{id}")),
            SubjectKind::Schedule => Some(format!("/schedules/{id}")),
            SubjectKind::Webhook => None,
        }
    }

    /// A target's subject: a host, or a cluster (also for its placements).
    pub fn target(kind: TargetKind, id: &str) -> Self {
        let kind = match kind {
            TargetKind::Host => SubjectKind::Host,
            TargetKind::Cluster | TargetKind::Workload => SubjectKind::Cluster,
        };
        Self::new(kind, id)
    }

    fn of(h: &HostSummary) -> Self {
        Self::target(h.kind, &h.id)
    }
}

/// Why something is on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// Enrolled and waiting for an administrator to approve it.
    Registration,
    /// Not connected for longer than [`OFFLINE_ATTENTION`].
    Offline,
    /// Running another agent version than the one it should converge to.
    VersionSkew,
    /// Its plan changed after its phase was approved.
    StalePlan,
    /// Failed in the live rollout.
    Failed,
    /// Held at baseline in the live rollout.
    Held,
    /// A health check reads `fail` now (required or informational).
    CheckFailing,
    /// Unhealthy with no failing check to name.
    Unhealthy,
    /// Its health is held.
    HealthHeld,
    /// A cluster's workloads have a required check failing.
    WorkloadsFailing,
    /// A rollout is paused.
    RolloutPaused,
    /// A schedule's latest occurrence started nothing.
    ScheduleSkipped,
    /// A host was refused a cluster identity and granted none since.
    IdentityRefused,
    /// A webhook's deliveries keep failing.
    WebhookFailing,
    /// A suspended module or workload is past the date set to review it.
    ReviewDue,
}

/// One thing that needs attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// Stable while the condition lasts (`host/pi-1/offline`).
    pub id: String,
    pub kind: AttentionKind,
    pub tone: AttentionTone,
    pub subject: AttentionSubject,
    /// What is wrong, without the subject's name (`agent 2.15.0, expected 2.16.0`).
    pub summary: String,
    /// When the condition began, when known (last seen, a check's reading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// When the server first listed it; set by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<DateTime<Utc>>,
}

impl AttentionItem {
    fn new(
        kind: AttentionKind,
        tone: AttentionTone,
        subject: AttentionSubject,
        key: &str,
        summary: impl Into<String>,
    ) -> Self {
        let mut id = format!(
            "{}/{}/{}",
            subject_kind_str(subject.kind),
            subject.id,
            kind_str(kind)
        );
        if !key.is_empty() {
            id.push('/');
            id.push_str(key);
        }
        Self {
            id,
            kind,
            tone,
            subject,
            summary: summary.into(),
            since: None,
            opened_at: None,
        }
    }

    fn since(mut self, at: Option<DateTime<Utc>>) -> Self {
        self.since = at;
        self
    }

    /// The UI page about the item's subject.
    pub fn path(&self) -> Option<String> {
        self.subject.path()
    }
}

/// `GET /api/v1/attention`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionList {
    /// Worst first: by tone, then by subject.
    pub items: Vec<AttentionItem>,
}

/// A host's latest check readings.
#[derive(Debug, Clone)]
pub struct TargetChecks<'a> {
    pub target: &'a str,
    pub checks: &'a [HostCheck],
}

/// A host refused an identity, with no grant for it since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRefusal {
    pub host: String,
    /// The cluster target, or the `fleet.identities` entry when `purpose`
    /// is empty.
    pub target: String,
    /// What the cluster credential was for; empty for an audience identity.
    pub purpose: String,
    pub reason: String,
    pub at: DateTime<Utc>,
}

/// A schedule whose latest occurrence started nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRun {
    pub schedule: String,
    pub due_at: DateTime<Utc>,
    pub reason: String,
}

/// A webhook whose deliveries have been failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookFailure {
    pub name: String,
    pub since: DateTime<Utc>,
    pub error: String,
    /// Deliveries waiting for it.
    pub queued: u32,
}

/// Everything [`build`] reads. Only `hosts` is required to say anything.
#[derive(Debug, Clone, Default)]
pub struct AttentionInputs<'a> {
    pub hosts: &'a [HostSummary],
    pub clusters: &'a [HostSummary],
    /// The agent version hosts should run.
    pub expected_version: Option<&'a str>,
    /// The live rollout, when one runs.
    pub rollout: Option<&'a RolloutDetail>,
    /// Each host's checks, filtered to those of the revision it runs.
    pub checks: &'a [TargetChecks<'a>],
    pub skipped: &'a [SkippedRun],
    pub refusals: &'a [IdentityRefusal],
    pub webhooks: &'a [WebhookFailure],
    /// The suspended modules and workloads.
    pub paused: &'a [crate::PausedUnit],
    /// Since when the server has been able to see agents connect: an agent
    /// is not offline for longer than the server has been up.
    pub watching_since: Option<DateTime<Utc>>,
}

/// The attention list as of `now`.
pub fn build(inputs: &AttentionInputs<'_>, now: DateTime<Utc>) -> Vec<AttentionItem> {
    use AttentionKind as K;
    use AttentionTone as T;
    let mut out: Vec<AttentionItem> = Vec::new();

    for h in inputs.hosts {
        let subject = AttentionSubject::of(h);
        let item = |kind, tone, key: &str, summary: String| {
            AttentionItem::new(kind, tone, subject.clone(), key, summary)
        };
        let pending = h.status == HostStatus::Pending;
        if pending {
            out.push(item(
                K::Registration,
                T::Info,
                "",
                "awaiting registration approval".into(),
            ));
        }
        let basis = match (h.last_seen, inputs.watching_since) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if h.connection == Connection::Offline
            && !pending
            && basis.is_none_or(|at| now - at > Duration::seconds(OFFLINE_ATTENTION))
        {
            let summary = if h.last_seen.is_some() {
                "agent offline"
            } else {
                "never connected"
            };
            out.push(item(K::Offline, T::Warn, "", summary.into()).since(h.last_seen));
        }
        // An offline host's last reading is not a statement about now.
        if !pending && h.connection == Connection::Online {
            let checks = inputs
                .checks
                .iter()
                .filter(|c| c.target == h.id)
                .flat_map(|c| c.checks.iter())
                .filter(|c| c.status == "fail");
            let mut required = false;
            for c in checks {
                required |= c.required;
                let name = format!("{}/{}", c.module, c.check);
                let tone = if c.required { T::Danger } else { T::Warn };
                let summary = match c.reason.as_deref().filter(|r| !r.is_empty()) {
                    Some(reason) => format!("{name} failing: {reason}"),
                    None => format!("{name} failing"),
                };
                out.push(item(K::CheckFailing, tone, &name, summary).since(Some(c.at)));
            }
            match h.health {
                HealthSummary::Failing if !required => out.push(
                    item(K::Unhealthy, T::Danger, "", "health checks failing".into())
                        .since(h.health_at),
                ),
                HealthSummary::Held => out.push(
                    item(K::HealthHeld, T::Warn, "", "health held".into()).since(h.health_at),
                ),
                _ => {}
            }
        }
        if let (Some(running), Some(expected)) =
            (h.agent_version.as_deref(), inputs.expected_version)
            && !same_version(running, expected)
        {
            out.push(item(
                K::VersionSkew,
                T::Warn,
                "",
                format!("agent {running}, expected {expected}"),
            ));
        }
    }

    for c in inputs.clusters {
        let failing = c
            .cluster_status
            .as_ref()
            .map(|s| s.failing.clone())
            .unwrap_or_default();
        if !failing.is_empty() {
            let n = failing.len();
            out.push(AttentionItem::new(
                K::WorkloadsFailing,
                T::Danger,
                AttentionSubject::of(c),
                "",
                format!(
                    "{n} {} failing: {}",
                    if n == 1 { "workload" } else { "workloads" },
                    failing.join(", ")
                ),
            ));
        }
    }

    if let Some(r) = inputs.rollout.filter(|r| !finished(r.summary.state)) {
        let rid = &r.summary.id;
        let name = r.summary.name.clone().unwrap_or_else(|| format!("#{rid}"));
        if r.summary.state == RolloutState::Paused {
            let summary = match r.summary.pause_reason.as_deref().filter(|s| !s.is_empty()) {
                Some(why) => format!("rollout {name} paused: {why}"),
                None => format!("rollout {name} paused"),
            };
            out.push(AttentionItem::new(
                K::RolloutPaused,
                T::Warn,
                AttentionSubject::new(SubjectKind::Rollout, rid.clone()),
                "",
                summary,
            ));
        }
        for phase in &r.phases {
            for entry in &phase.hosts {
                let (kind, tone, what) = match entry.state {
                    HostRolloutState::Failed => (K::Failed, T::Danger, "failed in"),
                    HostRolloutState::Held => (K::Held, T::Warn, "held at baseline in"),
                    _ if entry.needs_approval && phase.approved_by.is_some() => {
                        (K::StalePlan, T::Warn, "plan changed after approval of")
                    }
                    _ => continue,
                };
                let kind_of = inputs
                    .hosts
                    .iter()
                    .chain(inputs.clusters)
                    .find(|h| h.id == entry.host)
                    .map(|h| h.kind)
                    .unwrap_or(if entry.workload.is_some() {
                        TargetKind::Cluster
                    } else {
                        TargetKind::Host
                    });
                let subject = AttentionSubject::target(kind_of, &entry.host);
                let mut key = format!("{rid}/{}", phase.name);
                let mut summary = format!("{what} {}", phase.name);
                if let Some(w) = &entry.workload {
                    key.push('/');
                    key.push_str(w);
                    summary = format!("{w} {summary}");
                }
                // A node the rollout drained stays out of scheduling until
                // the host is healthy: say so where someone will look.
                let drained: Vec<&str> = entry
                    .nodes
                    .iter()
                    .filter(|n| n.state == crate::RolloutNodeState::LeftDrained)
                    .map(|n| n.cluster.as_str())
                    .collect();
                if !drained.is_empty() {
                    summary = format!(
                        "{summary}; its node stays drained on {}",
                        drained.join(", ")
                    );
                }
                let item = AttentionItem::new(kind, tone, subject, &key, summary)
                    .since(Some(entry.updated_at));
                if !out.iter().any(|i| i.id == item.id) {
                    out.push(item);
                }
            }
        }
    }

    for s in inputs.skipped {
        out.push(
            AttentionItem::new(
                K::ScheduleSkipped,
                T::Warn,
                AttentionSubject::new(SubjectKind::Schedule, s.schedule.clone()),
                &s.due_at.to_rfc3339(),
                format!(
                    "skipped the run due {}: {}",
                    s.due_at.format("%Y-%m-%d %H:%M UTC"),
                    s.reason
                ),
            )
            .since(Some(s.due_at)),
        );
    }

    for r in inputs.refusals {
        if now - r.at > Duration::seconds(REFUSAL_ATTENTION) {
            continue;
        }
        // An audience identity belongs to no cluster: it is the host's.
        let (subject, key, summary) = if r.purpose.is_empty() {
            (
                inputs
                    .hosts
                    .iter()
                    .find(|h| h.id == r.host)
                    .map(AttentionSubject::of)
                    .unwrap_or_else(|| AttentionSubject::new(SubjectKind::Host, r.host.clone())),
                r.target.clone(),
                format!("refused the {} identity: {}", r.target, r.reason),
            )
        } else {
            (
                inputs
                    .clusters
                    .iter()
                    .find(|c| c.id == r.target)
                    .map(AttentionSubject::of)
                    .unwrap_or_else(|| {
                        AttentionSubject::new(SubjectKind::Cluster, r.target.clone())
                    }),
                format!("{}/{}", r.host, r.purpose),
                format!(
                    "{} was refused a {} identity: {}",
                    r.host, r.purpose, r.reason
                ),
            )
        };
        out.push(
            AttentionItem::new(K::IdentityRefused, T::Warn, subject, &key, summary)
                .since(Some(r.at)),
        );
    }

    for w in inputs.webhooks {
        let queued = match w.queued {
            0 => String::new(),
            1 => ", 1 delivery queued".into(),
            n => format!(", {n} deliveries queued"),
        };
        out.push(
            AttentionItem::new(
                K::WebhookFailing,
                T::Warn,
                AttentionSubject::new(SubjectKind::Webhook, w.name.clone()),
                "",
                format!("deliveries failing: {}{queued}", w.error),
            )
            .since(Some(w.since)),
        );
    }

    for unit in inputs.paused {
        let Some(due) = unit.review_at.filter(|at| *at <= now) else {
            continue;
        };
        let cluster = inputs.clusters.iter().any(|c| c.id == unit.target);
        let kind = if cluster {
            SubjectKind::Cluster
        } else {
            SubjectKind::Host
        };
        out.push(
            AttentionItem::new(
                K::ReviewDue,
                T::Warn,
                AttentionSubject::new(kind, unit.target.clone()),
                &unit.unit,
                format!(
                    "{} is suspended and was to be reviewed by {}: {}",
                    unit.unit,
                    due.format("%Y-%m-%d %H:%M UTC"),
                    unit.note
                ),
            )
            .since(Some(due)),
        );
    }

    sort(&mut out);
    out
}

/// Worst first: by the subject's worst tone, then subjects with more items,
/// then by subject; within a subject, by tone.
pub fn sort(items: &mut [AttentionItem]) {
    use std::collections::HashMap;
    let mut worst: HashMap<AttentionSubject, (AttentionTone, usize)> = HashMap::new();
    for i in items.iter() {
        let e = worst
            .entry(i.subject.clone())
            .or_insert((AttentionTone::Info, 0));
        e.0 = e.0.min(i.tone);
        e.1 += 1;
    }
    items.sort_by(|a, b| {
        let (wa, na) = worst[&a.subject];
        let (wb, nb) = worst[&b.subject];
        wa.cmp(&wb)
            .then(nb.cmp(&na))
            .then(a.subject.cmp(&b.subject))
            .then(a.tone.cmp(&b.tone))
    });
}

/// Whether `a` and `b` name the same release (`v2.3.0` is `2.3.0`).
fn same_version(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches('v') == b.trim().trim_start_matches('v')
}

fn finished(state: RolloutState) -> bool {
    matches!(
        state,
        RolloutState::Completed
            | RolloutState::RolledBack
            | RolloutState::Aborted
            | RolloutState::Skipped
    )
}

fn subject_kind_str(k: SubjectKind) -> &'static str {
    match k {
        SubjectKind::Host => "host",
        SubjectKind::Cluster => "cluster",
        SubjectKind::Rollout => "rollout",
        SubjectKind::Schedule => "schedule",
        SubjectKind::Webhook => "webhook",
    }
}

fn kind_str(k: AttentionKind) -> &'static str {
    match k {
        AttentionKind::Registration => "registration",
        AttentionKind::Offline => "offline",
        AttentionKind::VersionSkew => "version_skew",
        AttentionKind::StalePlan => "stale_plan",
        AttentionKind::Failed => "failed",
        AttentionKind::Held => "held",
        AttentionKind::CheckFailing => "check_failing",
        AttentionKind::Unhealthy => "unhealthy",
        AttentionKind::HealthHeld => "health_held",
        AttentionKind::WorkloadsFailing => "workloads_failing",
        AttentionKind::RolloutPaused => "rollout_paused",
        AttentionKind::ScheduleSkipped => "schedule_skipped",
        AttentionKind::IdentityRefused => "identity_refused",
        AttentionKind::WebhookFailing => "webhook_failing",
        AttentionKind::ReviewDue => "review_due",
    }
}

/// Percent-encodes a path segment.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClusterStatus, PhaseState, RolloutHost, RolloutPhase, RolloutSummary};
    use rstest::rstest;

    fn host(id: &str) -> HostSummary {
        HostSummary {
            id: id.into(),
            kind: TargetKind::Host,
            cluster: None,
            cluster_status: None,
            status: HostStatus::Active,
            connection: Connection::Online,
            agent_labels: Default::default(),
            labels: Default::default(),
            effective_labels: Default::default(),
            reserved_labels: Vec::new(),
            verified_tags: None,
            groups: vec![],
            platform: None,
            arch: None,
            agent_version: Some("2.0.0".into()),
            applied_revision: None,
            health: HealthSummary::Healthy,
            health_at: None,
            drifted: false,
            drift_at: None,
            first_seen: Utc::now(),
            last_seen: Some(Utc::now()),
            notes: String::new(),
            upgrade: None,
        }
    }

    fn entry(name: &str, state: HostRolloutState, stale: bool) -> RolloutHost {
        RolloutHost {
            host: name.into(),
            phase: "home".into(),
            wave: None,
            state,
            plan_digest: None,
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            baseline_failing: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            reboot: None,
            kind: Default::default(),
            step: 0,
            workload: None,
            executor: None,
            bake: None,
            preview: false,
            drifted_from: None,
            removal: false,
            earlier: vec![],
            total: None,
            waiting: None,
            needs_approval: stale,
            needs_attention: false,
            error_detail: None,
            run_id: None,
            nodes: vec![],
        }
    }

    fn rollout(state: RolloutState, hosts: Vec<RolloutHost>) -> RolloutDetail {
        RolloutDetail {
            summary: RolloutSummary {
                id: "42".into(),
                name: None,
                revision: "r".into(),
                revision_message: "m".into(),
                state,
                started_at: Utc::now(),
                finished_at: None,
                triggered_by: "c".into(),
                hosts_total: hosts.len() as u32,
                hosts_done: 0,
                hosts_failed: 0,
                current_phase: None,
                pause_reason: Some("operator".into()),
                end_reason: None,
                superseded_by: None,
                trace_id: None,
                trace_url: None,
                action: None,
                changes: None,
                reapply: false,
            },
            phases: vec![RolloutPhase {
                name: "home".into(),
                state: PhaseState::Running,
                parallel: "1".into(),
                approval: "required".into(),
                bake_seconds: 0,
                max_failures: "1".into(),
                virtual_stage: false,
                hosts,
                approved_by: Some("alice".into()),
                approved_at: None,
                bake_until: None,
                health_checks: vec![],
                stages: 1,
                unchanged: 0,
            }],
            plans: vec![],
            fleet: None,
            limits: Vec::new(),
            events: vec![],
        }
    }

    fn check(module: &str, name: &str, status: &str, required: bool) -> HostCheck {
        HostCheck {
            module: module.into(),
            check: name.into(),
            kind: "command".into(),
            required,
            status: status.into(),
            reason: None,
            scope: "drift".into(),
            at: Utc::now(),
            history: vec![],
        }
    }

    fn ids(items: &[AttentionItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn hosts_are_listed_for_registration_offline_skew_and_rollout_trouble() {
        let now = Utc::now();
        let mut pending = host("new-1");
        pending.status = HostStatus::Pending;
        let mut gone = host("az-1");
        gone.connection = Connection::Offline;
        gone.last_seen = Some(now - Duration::minutes(25));
        let mut blip = host("pi-9");
        blip.connection = Connection::Offline;
        blip.last_seen = Some(now - Duration::minutes(2));
        let mut skewed = host("lab-3");
        skewed.agent_version = Some("1.9.0".into());
        let mut current = host("pi-2");
        current.agent_version = Some("v2.0.0".into());
        let r = rollout(
            RolloutState::Rolling,
            vec![
                entry("pi-5", HostRolloutState::Failed, false),
                entry("lab-3", HostRolloutState::Planned, true),
                entry("pi-2", HostRolloutState::Planned, false),
            ],
        );
        let hosts = [current, host("pi-5"), pending, gone, blip, skewed];
        let list = build(
            &AttentionInputs {
                hosts: &hosts,
                expected_version: Some("2.0.0"),
                rollout: Some(&r),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            ids(&list),
            vec![
                "host/pi-5/failed/42/home",
                "host/lab-3/version_skew",
                "host/lab-3/stale_plan/42/home",
                "host/az-1/offline",
                "host/new-1/registration",
            ]
        );
        assert_eq!(list[1].summary, "agent 1.9.0, expected 2.0.0");
        assert_eq!(list[3].since, Some(now - Duration::minutes(25)));
        assert_eq!(list[0].path().as_deref(), Some("/hosts/pi-5"));
    }

    #[test]
    fn a_failed_host_says_its_node_stays_drained() {
        let now = Utc::now();
        let mut failed = entry("pi-5", HostRolloutState::Failed, false);
        failed.nodes = vec![crate::RolloutNode {
            cluster: "labk3s".into(),
            state: crate::RolloutNodeState::LeftDrained,
            reasons: vec!["k3s changes and disrupts the node".into()],
            summary: None,
            since: now,
        }];
        let r = rollout(RolloutState::Paused, vec![failed]);
        let hosts = [host("pi-5")];
        let list = build(
            &AttentionInputs {
                hosts: &hosts,
                rollout: Some(&r),
                ..Default::default()
            },
            now,
        );
        let item = list
            .iter()
            .find(|i| i.id == "host/pi-5/failed/42/home")
            .expect("the failed host is listed");
        assert_eq!(
            item.summary,
            "failed in home; its node stays drained on labk3s"
        );
    }

    #[test]
    fn an_agent_is_not_offline_for_longer_than_the_server_watched() {
        let now = Utc::now();
        let mut gone = host("az-1");
        gone.connection = Connection::Offline;
        gone.last_seen = Some(now - Duration::hours(3));
        let mut never = host("az-2");
        never.connection = Connection::Offline;
        never.last_seen = None;
        let hosts = [gone, never];
        let at = |since| {
            build(
                &AttentionInputs {
                    hosts: &hosts,
                    watching_since: Some(since),
                    ..Default::default()
                },
                now,
            )
        };
        assert!(
            at(now - Duration::minutes(1)).is_empty(),
            "the server just started"
        );
        assert_eq!(
            ids(&at(now - Duration::minutes(11))),
            vec!["host/az-1/offline", "host/az-2/offline"]
        );
    }

    #[rstest]
    #[case::required(true, AttentionTone::Danger)]
    #[case::informational(false, AttentionTone::Warn)]
    fn each_failing_check_of_an_online_host_is_an_item(
        #[case] required: bool,
        #[case] tone: AttentionTone,
    ) {
        let mut h = host("pi-1");
        h.health = if required {
            HealthSummary::Failing
        } else {
            HealthSummary::Healthy
        };
        let checks = [
            check("apt", "updates", "fail", required),
            check("app", "api", "pass", true),
        ];
        let hosts = [h];
        let list = build(
            &AttentionInputs {
                hosts: &hosts,
                checks: &[TargetChecks {
                    target: "pi-1",
                    checks: &checks,
                }],
                ..Default::default()
            },
            Utc::now(),
        );
        assert_eq!(ids(&list), vec!["host/pi-1/check_failing/apt/updates"]);
        assert_eq!(list[0].tone, tone);
    }

    #[test]
    fn health_without_a_named_check_and_cluster_workloads_are_listed() {
        let mut failing = host("pi-1");
        failing.health = HealthSummary::Failing;
        let mut held = host("pi-2");
        held.health = HealthSummary::Held;
        let mut stale = host("pi-3");
        stale.connection = Connection::Offline;
        stale.health = HealthSummary::Failing;
        let mut cluster = host("home-nomad");
        cluster.kind = TargetKind::Cluster;
        cluster.cluster_status = Some(ClusterStatus {
            failing: vec!["api".into(), "web".into()],
            ..Default::default()
        });
        let hosts = [failing, held, stale];
        let clusters = [cluster];
        let list = build(
            &AttentionInputs {
                hosts: &hosts,
                clusters: &clusters,
                ..Default::default()
            },
            Utc::now(),
        );
        assert_eq!(
            ids(&list),
            vec![
                "host/pi-1/unhealthy",
                "cluster/home-nomad/workloads_failing",
                "host/pi-2/health_held",
            ]
        );
        assert!(list[1].summary.contains("api, web"));
        assert_eq!(list[1].path().as_deref(), Some("/clusters/home-nomad"));
    }

    #[test]
    fn paused_rollouts_skipped_schedules_refusals_and_webhooks_are_listed() {
        let now = Utc::now();
        let r = rollout(RolloutState::Paused, vec![]);
        let skipped = [SkippedRun {
            schedule: "backup".into(),
            due_at: now,
            reason: "a rollout was under way".into(),
        }];
        let refusal = |at| IdentityRefusal {
            host: "pi-1".into(),
            target: "home-nomad".into(),
            purpose: "deploy".into(),
            reason: "not an executor".into(),
            at,
        };
        let refusals = [
            refusal(now),
            IdentityRefusal {
                target: "old".into(),
                ..refusal(now - Duration::days(2))
            },
            // An audience identity (`fleet.identities`) is the host's own.
            IdentityRefusal {
                target: "vault".into(),
                purpose: String::new(),
                ..refusal(now)
            },
        ];
        let webhooks = [WebhookFailure {
            name: "automate".into(),
            since: now,
            error: "connection refused".into(),
            queued: 3,
        }];
        let suspended = |unit: &str, review_at| crate::PausedUnit {
            target: "pi-1".into(),
            unit: unit.into(),
            note: "disk swap".into(),
            by: "ops".into(),
            at: now - Duration::days(3),
            review_at,
        };
        let paused = [
            suspended("backup", Some(now - Duration::hours(1))),
            suspended("later", Some(now + Duration::hours(1))),
            suspended("open-ended", None),
        ];
        let list = build(
            &AttentionInputs {
                rollout: Some(&r),
                skipped: &skipped,
                refusals: &refusals,
                webhooks: &webhooks,
                paused: &paused,
                ..Default::default()
            },
            now,
        );
        let kinds: Vec<_> = list
            .iter()
            .map(|i| (i.kind, i.subject.id.as_str()))
            .collect();
        assert_eq!(kinds.len(), 6, "{kinds:?}");
        let due = list
            .iter()
            .find(|i| i.kind == AttentionKind::ReviewDue)
            .expect("a review date that passed is listed");
        assert_eq!(due.path().as_deref(), Some("/hosts/pi-1"));
        assert!(due.summary.contains("backup") && due.summary.contains("disk swap"));
        for want in [
            (AttentionKind::RolloutPaused, "42"),
            (AttentionKind::ScheduleSkipped, "backup"),
            (AttentionKind::IdentityRefused, "home-nomad"),
            (AttentionKind::IdentityRefused, "pi-1"),
            (AttentionKind::WebhookFailing, "automate"),
        ] {
            assert!(kinds.contains(&want), "{want:?} in {kinds:?}");
        }
        let hook = list
            .iter()
            .find(|i| i.kind == AttentionKind::WebhookFailing)
            .unwrap();
        assert_eq!(hook.path(), None);
        assert!(hook.summary.contains("3 deliveries queued"));
    }

    #[test]
    fn a_finished_rollout_needs_no_attention() {
        let r = rollout(
            RolloutState::Completed,
            vec![entry("pi-5", HostRolloutState::Failed, false)],
        );
        let list = build(
            &AttentionInputs {
                rollout: Some(&r),
                ..Default::default()
            },
            Utc::now(),
        );
        assert!(list.is_empty(), "{list:?}");
    }

    #[test]
    fn ids_are_stable_across_passes() {
        let mut h = host("pi-1");
        h.agent_version = Some("1.0.0".into());
        let hosts = [h];
        let inputs = AttentionInputs {
            hosts: &hosts,
            expected_version: Some("2.0.0"),
            ..Default::default()
        };
        let a = build(&inputs, Utc::now());
        let b = build(&inputs, Utc::now() + Duration::minutes(5));
        assert_eq!(ids(&a), ids(&b));
    }
}
