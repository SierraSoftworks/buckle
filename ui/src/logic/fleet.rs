//! The fleet at a glance (design §07, §12): how many hosts and workloads
//! are healthy, deploying, failing, held, offline or drifted, and which
//! agents need someone to look at them.

use std::collections::BTreeMap;

use buckle_api::{
    Connection, HealthSummary, HostRolloutState, HostStatus, HostSummary, RolloutDetail,
    UpgradeState,
};
use chrono::{DateTime, Duration, Utc};

/// How long an agent may be offline before it needs attention.
pub const OFFLINE_ATTENTION: i64 = 10 * 60;

/// Where a host stands now, by its health and connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    Healthy,
    Failing,
    Held,
    Offline,
    Unknown,
}

impl Bucket {
    /// The hosts-table filter for it.
    pub fn filter(self) -> &'static str {
        match self {
            Bucket::Healthy => "healthy",
            Bucket::Failing => "failing",
            Bucket::Held => "held",
            Bucket::Offline => "offline",
            Bucket::Unknown => "unknown",
        }
    }
}

/// Which health bucket a host falls in. An offline host's last reading is not
/// a statement about now, so offline wins over health; drift is orthogonal
/// and counted separately by [`rollup`].
pub fn bucket(host: &HostSummary) -> Bucket {
    if host.connection == Connection::Offline {
        return Bucket::Offline;
    }
    match host.health {
        HealthSummary::Healthy => Bucket::Healthy,
        HealthSummary::Failing => Bucket::Failing,
        HealthSummary::Held => Bucket::Held,
        HealthSummary::Unknown => Bucket::Unknown,
    }
}

/// Whether a host passes a hosts-table filter named by [`Bucket::filter`] (or
/// `pending`/`paused` for registration status).
pub fn matches_filter(host: &HostSummary, filter: &str) -> bool {
    match filter {
        "" | "all" => true,
        "pending" => host.status == HostStatus::Pending,
        "paused" => host.status == HostStatus::Paused,
        "drifted" => host.drifted,
        other => bucket(host).filter() == other,
    }
}

/// The fleet right now: every registered host and every workload of the
/// clusters, each counted once by its state. Hosts still awaiting
/// registration count apart (`pending`); drift is orthogonal and counted
/// on top.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetStates {
    pub hosts: usize,
    pub workloads: usize,
    pub healthy: usize,
    pub deploying: usize,
    pub held: usize,
    pub failing: usize,
    pub offline: usize,
    pub unknown: usize,
    pub pending: usize,
    pub drifted: usize,
}

impl FleetStates {
    /// Everything the bar spans.
    pub fn total(&self) -> usize {
        self.hosts + self.workloads + self.pending
    }
}

pub fn fleet_states(hosts: &[HostSummary], clusters: &[HostSummary]) -> FleetStates {
    let mut s = FleetStates::default();
    let add = |s: &mut FleetStates, b: Bucket, n: usize| match b {
        Bucket::Healthy => s.healthy += n,
        Bucket::Failing => s.failing += n,
        Bucket::Held => s.held += n,
        Bucket::Offline => s.offline += n,
        Bucket::Unknown => s.unknown += n,
    };
    for h in hosts {
        if h.status == HostStatus::Pending {
            s.pending += 1;
            continue;
        }
        s.hosts += 1;
        s.drifted += h.drifted as usize;
        add(&mut s, bucket(h), 1);
    }
    for c in clusters {
        let status = c.cluster_status.clone().unwrap_or_default();
        let n = status.workloads as usize;
        s.workloads += n;
        // A cluster has no agent of its own (its connection says nothing):
        // with no host to speak for it, its workloads have no reading.
        if status.executor.is_none() {
            add(&mut s, Bucket::Unknown, n);
            continue;
        }
        let failing = status.failing.len().min(n);
        let deploying = status
            .deploying
            .iter()
            .filter(|d| !status.failing.contains(&d.workload))
            .count()
            .min(n - failing);
        s.failing += failing;
        s.deploying += deploying;
        add(&mut s, Bucket::Healthy, n - failing - deploying);
    }
    s
}

/// Why an agent is on the attention list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Concern {
    /// Enrolled and waiting for an administrator to approve it.
    Registration,
    /// Not connected for longer than [`OFFLINE_ATTENTION`].
    Offline { since: Option<DateTime<Utc>> },
    /// Running a different agent version from the one it should converge to.
    VersionSkew { running: String, expected: String },
    /// Its plan changed after its phase was approved, so it waits for a
    /// fresh approval of its own.
    StalePlan { phase: String },
    /// Failed in the active rollout.
    Failed { phase: String },
    /// Held at baseline in the active rollout: unhealthy before Buckle touched it.
    Held { phase: String },
    /// Connected, with a required health check failing now.
    Unhealthy,
    /// Connected, with its health held.
    HealthHeld,
    /// A cluster whose workloads have a required check failing now.
    WorkloadsFailing { workloads: Vec<String> },
}

impl Concern {
    /// How much it asks for: a failure first, a registration last.
    pub fn tone(&self) -> super::tone::Tone {
        use super::tone::Tone;
        match self {
            Concern::Failed { .. } | Concern::Unhealthy | Concern::WorkloadsFailing { .. } => {
                Tone::Danger
            }
            Concern::Held { .. }
            | Concern::HealthHeld
            | Concern::StalePlan { .. }
            | Concern::VersionSkew { .. }
            | Concern::Offline { .. } => Tone::Warn,
            Concern::Registration => Tone::Info,
        }
    }

    fn severity(&self) -> u8 {
        use super::tone::Tone;
        match self.tone() {
            Tone::Danger => 0,
            Tone::Warn => 1,
            _ => 2,
        }
    }

    pub fn describe(&self, now: DateTime<Utc>) -> String {
        match self {
            Concern::Registration => "awaiting registration approval".into(),
            Concern::Offline { since } => match since {
                Some(at) => format!(
                    "offline for {}",
                    super::format::short_duration(now.signed_duration_since(*at).num_seconds())
                ),
                None => "never connected".into(),
            },
            Concern::VersionSkew { running, expected } => {
                format!("agent {running}, expected {expected}")
            }
            Concern::StalePlan { phase } => format!("plan changed after {phase} was approved"),
            Concern::Failed { phase } => format!("failed in {phase}"),
            Concern::Held { phase } => format!("held at baseline in {phase}"),
            Concern::Unhealthy => "health checks failing".into(),
            Concern::HealthHeld => "health held".into(),
            Concern::WorkloadsFailing { workloads } => {
                format!(
                    "{} failing",
                    super::format::count(workloads.len(), "workload", "workloads")
                ) + &format!(": {}", workloads.join(", "))
            }
        }
    }
}

/// The version agents should be running: the server's own when it says, or
/// otherwise the most common version among connected agents (ties go to the
/// lexically greatest, which is the newer of two otherwise equal releases).
pub fn expected_version<'a>(
    hosts: &'a [HostSummary],
    server_version: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(v) = server_version {
        return Some(v);
    }
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for v in hosts
        .iter()
        .filter(|h| h.connection == Connection::Online)
        .filter_map(|h| h.agent_version.as_deref())
    {
        *tally.entry(v).or_default() += 1;
    }
    tally
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)))
        .map(|(v, _)| v)
}

/// Whether `a` and `b` name the same release (`v2.3.0` is `2.3.0`), as the
/// server compares them.
pub fn same_version(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches('v') == b.trim().trim_start_matches('v')
}

/// The agents behind `target`, split by whether "Upgrade all" can reach them.
#[derive(Debug, Default, PartialEq)]
pub struct Behind<'a> {
    /// Online and not already upgrading to `target`: asked to upgrade.
    pub upgradable: Vec<&'a HostSummary>,
    /// Offline: skipped until they reconnect.
    pub offline: Vec<&'a HostSummary>,
}

/// The fleet's agents not running `target`. Registrations awaiting approval
/// are not part of the fleet yet and are left out, as are agents already
/// upgrading to `target` (a failed upgrade counts as behind, so it is retried).
pub fn agents_behind<'a>(hosts: &'a [HostSummary], target: &str) -> Behind<'a> {
    let mut out = Behind::default();
    for h in hosts {
        let current = h
            .agent_version
            .as_deref()
            .is_some_and(|v| same_version(v, target));
        let upgrading = h.upgrade.as_ref().is_some_and(|u| {
            u.state == UpgradeState::Requested && same_version(&u.version, target)
        });
        if h.status == HostStatus::Pending || current || upgrading {
            continue;
        }
        match h.connection {
            Connection::Online => out.upgradable.push(h),
            Connection::Offline => out.offline.push(h),
        }
    }
    out
}

/// The hosts and clusters needing attention, worst first: by their most
/// severe concern, then by how many they have, then by name.
pub fn attention(
    hosts: &[HostSummary],
    clusters: &[HostSummary],
    server_version: Option<&str>,
    rollout: Option<&RolloutDetail>,
    now: DateTime<Utc>,
) -> Vec<(String, Vec<Concern>)> {
    let expected = expected_version(hosts, server_version);
    let mut out: BTreeMap<String, Vec<Concern>> = BTreeMap::new();

    for h in hosts {
        let mut concerns = Vec::new();
        if h.status == HostStatus::Pending {
            concerns.push(Concern::Registration);
        }
        if h.connection == Connection::Offline
            && h.status != HostStatus::Pending
            && h.last_seen
                .is_none_or(|at| now - at > Duration::seconds(OFFLINE_ATTENTION))
        {
            concerns.push(Concern::Offline { since: h.last_seen });
        }
        // An offline host's last reading is not a statement about now.
        if h.status != HostStatus::Pending && h.connection == Connection::Online {
            match h.health {
                HealthSummary::Failing => concerns.push(Concern::Unhealthy),
                HealthSummary::Held => concerns.push(Concern::HealthHeld),
                _ => {}
            }
        }
        if let (Some(running), Some(expected)) = (h.agent_version.as_deref(), expected)
            && running != expected
        {
            concerns.push(Concern::VersionSkew {
                running: running.into(),
                expected: expected.into(),
            });
        }
        if !concerns.is_empty() {
            out.insert(h.id.clone(), concerns);
        }
    }

    for c in clusters {
        let failing = c
            .cluster_status
            .as_ref()
            .map(|s| s.failing.clone())
            .unwrap_or_default();
        if !failing.is_empty() {
            out.entry(c.id.clone())
                .or_default()
                .push(Concern::WorkloadsFailing { workloads: failing });
        }
    }

    if let Some(rollout) = rollout {
        for phase in &rollout.phases {
            for entry in &phase.hosts {
                let concern = match entry.state {
                    HostRolloutState::Failed => Some(Concern::Failed {
                        phase: phase.name.clone(),
                    }),
                    HostRolloutState::Held => Some(Concern::Held {
                        phase: phase.name.clone(),
                    }),
                    _ if entry.needs_approval && phase.approved_by.is_some() => {
                        Some(Concern::StalePlan {
                            phase: phase.name.clone(),
                        })
                    }
                    _ => None,
                };
                if let Some(c) = concern {
                    let list = out.entry(entry.host.clone()).or_default();
                    if !list.contains(&c) {
                        list.push(c);
                    }
                }
            }
        }
    }

    let mut list: Vec<_> = out.into_iter().collect();
    let worst = |c: &[Concern]| c.iter().map(Concern::severity).min().unwrap_or(u8::MAX);
    list.sort_by(|a, b| {
        worst(&a.1)
            .cmp(&worst(&b.1))
            .then(b.1.len().cmp(&a.1.len()))
            .then(a.0.cmp(&b.0))
    });
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{PhaseState, RolloutHost, RolloutPhase, RolloutState, RolloutSummary};

    fn host(id: &str) -> HostSummary {
        HostSummary {
            id: id.into(),
            kind: Default::default(),
            cluster: None,
            status: HostStatus::Active,
            connection: Connection::Online,
            agent_labels: BTreeMap::new(),
            labels: BTreeMap::new(),
            effective_labels: BTreeMap::new(),
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
            cluster_status: None,
        }
    }

    fn cluster(
        workloads: u32,
        executor: Option<&str>,
        failing: &[&str],
        deploying: &[&str],
    ) -> HostSummary {
        HostSummary {
            kind: buckle_api::TargetKind::Cluster,
            connection: Connection::Offline,
            cluster_status: Some(buckle_api::ClusterStatus {
                executor: executor.map(Into::into),
                workloads,
                failing: failing.iter().map(|w| w.to_string()).collect(),
                deploying: deploying
                    .iter()
                    .map(|w| buckle_api::Deploying {
                        workload: w.to_string(),
                        progress: None,
                    })
                    .collect(),
                ..Default::default()
            }),
            ..host("c")
        }
    }

    #[test]
    fn fleet_states_count_each_host_and_workload_once() {
        let mut failing = host("b");
        failing.health = HealthSummary::Failing;
        let mut offline = host("c");
        offline.connection = Connection::Offline;
        offline.health = HealthSummary::Failing; // a stale reading is not failing now
        let mut drifted = host("d");
        drifted.drifted = true;
        let mut pending = host("e");
        pending.status = HostStatus::Pending;
        let hosts = [host("a"), failing, offline, drifted, pending];
        let clusters = [
            // `api` fails while it deploys: counted once, as failing.
            cluster(5, Some("seer"), &["api"], &["api", "web"]),
            cluster(2, None, &[], &[]),
        ];

        let s = fleet_states(&hosts, &clusters);
        assert_eq!((s.hosts, s.workloads, s.pending), (4, 7, 1));
        assert_eq!(s.healthy, 2 + 3);
        assert_eq!(
            (s.failing, s.deploying, s.offline, s.unknown),
            (1 + 1, 1, 1, 2)
        );
        assert_eq!(s.drifted, 1);
        let parts =
            s.healthy + s.deploying + s.held + s.failing + s.offline + s.unknown + s.pending;
        assert_eq!(parts, s.total(), "the parts make up the whole");
    }

    #[test]
    fn hosts_table_filters_match_the_tiles() {
        let mut offline = host("c");
        offline.connection = Connection::Offline;
        assert!(matches_filter(&offline, "offline"));
        assert!(!matches_filter(&offline, "healthy"));
        assert!(matches_filter(&offline, "all"));
        let mut pending = host("p");
        pending.status = HostStatus::Pending;
        assert!(matches_filter(&pending, "pending"));
    }

    #[test]
    fn the_expected_version_is_the_servers_or_the_fleets_majority() {
        let mut old = host("b");
        old.agent_version = Some("1.9.0".into());
        let hosts = vec![host("a"), host("c"), old];
        assert_eq!(expected_version(&hosts, Some("2.1.0")), Some("2.1.0"));
        assert_eq!(expected_version(&hosts, None), Some("2.0.0"));
    }

    #[test]
    fn agents_behind_the_target_are_upgraded_when_reachable() {
        use Connection::{Offline, Online};
        use HostStatus::{Active, Paused, Pending};
        // `Some(online)` when the host is behind, `None` when it is left out.
        for (case, connection, status, running, upgrade, want) in [
            ("behind", Online, Active, "1.9.0", None, Some(true)),
            ("paused", Online, Paused, "1.9.0", None, Some(true)),
            (
                "failed upgrade",
                Online,
                Active,
                "1.9.0",
                Some(UpgradeState::Failed),
                Some(true),
            ),
            ("offline", Offline, Active, "1.9.0", None, Some(false)),
            ("current", Online, Active, "v2.1.0", None, None),
            (
                "upgrading",
                Online,
                Active,
                "1.9.0",
                Some(UpgradeState::Requested),
                None,
            ),
            ("pending", Online, Pending, "1.9.0", None, None),
        ] {
            let mut h = host(case);
            h.connection = connection;
            h.status = status;
            h.agent_version = Some(running.into());
            h.upgrade = upgrade.map(|state| buckle_api::AgentUpgrade {
                version: "2.1.0".into(),
                state,
                requested_at: Utc::now(),
                requested_by: "ben".into(),
                error: None,
            });
            let hosts = [h];
            let behind = agents_behind(&hosts, "2.1.0");
            assert_eq!(
                behind.upgradable.len(),
                usize::from(want == Some(true)),
                "{case}"
            );
            assert_eq!(
                behind.offline.len(),
                usize::from(want == Some(false)),
                "{case}"
            );
        }
    }

    #[test]
    fn attention_lists_registration_offline_skew_and_rollout_trouble() {
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

        let entry = |name: &str, state, stale| RolloutHost {
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
            error: None,
            updated_at: now,
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
            needs_approval: stale,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        };
        let rollout = RolloutDetail {
            summary: RolloutSummary {
                id: "42".into(),
                name: None,
                revision: "r".into(),
                revision_message: "m".into(),
                state: RolloutState::Rolling,
                started_at: now,
                finished_at: None,
                triggered_by: "c".into(),
                hosts_total: 3,
                hosts_done: 0,
                hosts_failed: 1,
                current_phase: None,
                pause_reason: None,
                end_reason: None,
                superseded_by: None,
                trace_id: None,
                trace_url: None,
                action: None,
                changes: None,
            },
            phases: vec![RolloutPhase {
                name: "home".into(),
                state: PhaseState::Running,
                parallel: "1".into(),
                approval: "required".into(),
                bake_seconds: 0,
                max_failures: "1".into(),
                virtual_stage: false,
                hosts: vec![
                    entry("pi-5", HostRolloutState::Failed, false),
                    entry("lab-3", HostRolloutState::Planned, true),
                    entry("pi-2", HostRolloutState::Planned, false),
                ],
                approved_by: Some("alice".into()),
                approved_at: None,
                bake_until: None,
                health_checks: vec![],
                stages: 1,
                unchanged: 0,
            }],
            plans: vec![],
            fleet: None,
            events: vec![],
        };

        let hosts = vec![host("pi-2"), host("pi-5"), pending, gone, blip, skewed];
        let list = attention(&hosts, &[], Some("2.0.0"), Some(&rollout), now);
        let names: Vec<&str> = list.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(names, vec!["pi-5", "lab-3", "az-1", "new-1"]);
        assert_eq!(
            list[1].1,
            vec![
                Concern::VersionSkew {
                    running: "1.9.0".into(),
                    expected: "2.0.0".into()
                },
                Concern::StalePlan {
                    phase: "home".into()
                }
            ]
        );
        assert_eq!(list[2].1[0].describe(now), "offline for 25m");
    }

    #[test]
    fn attention_lists_unhealthy_hosts_and_failing_workloads() {
        let now = Utc::now();
        let mut failing = host("pi-1");
        failing.health = HealthSummary::Failing;
        let mut held = host("pi-2");
        held.health = HealthSummary::Held;
        let mut stale = host("pi-3");
        stale.connection = Connection::Offline;
        stale.health = HealthSummary::Failing;
        stale.last_seen = Some(now);
        let clusters = [
            cluster(3, Some("pi-1"), &["api", "web"], &[]),
            HostSummary {
                id: "quiet".into(),
                ..cluster(1, Some("pi-1"), &[], &[])
            },
        ];

        let list = attention(
            &[failing, held, stale, host("pi-4")],
            &clusters,
            Some("2.0.0"),
            None,
            now,
        );
        let find = |id: &str| list.iter().find(|(h, _)| h == id).map(|(_, c)| c.clone());
        assert_eq!(find("pi-1"), Some(vec![Concern::Unhealthy]));
        assert_eq!(find("pi-2"), Some(vec![Concern::HealthHeld]));
        assert_eq!(
            find("pi-3"),
            None,
            "an offline host's stale reading is not a concern"
        );
        assert_eq!(find("pi-4"), None);
        assert_eq!(find("quiet"), None);
        let workloads = find("c").expect("the cluster with failing workloads");
        assert_eq!(workloads[0].tone(), crate::logic::tone::Tone::Danger);
        assert!(workloads[0].describe(now).contains("api, web"));
    }
}
