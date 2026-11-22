//! The fleet at a glance (design §07, §12): how many hosts and workloads
//! are healthy, deploying, failing, held, offline or drifted, and which
//! agents need someone to look at them.

use std::collections::BTreeMap;

use buckle_api::{Connection, HealthSummary, HostStatus, HostSummary, UpgradeState};

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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
}
