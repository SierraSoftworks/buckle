//! The Overview's roll-up of the fleet (design §07, §12): how many hosts are
//! healthy, failing, held, offline or drifted, how old those readings are, and
//! which agents need someone to look at them.

use std::collections::BTreeMap;

use buckle_api::{
    Connection, HealthSummary, HostRolloutState, HostStatus, HostSummary, RolloutDetail,
    UpgradeState,
};
use chrono::{DateTime, Duration, Utc};

/// How long an agent may be offline before it needs attention.
pub const OFFLINE_ATTENTION: i64 = 10 * 60;

/// One tile of the roll-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    Healthy,
    Failing,
    Held,
    Offline,
    Drifted,
    Unknown,
}

impl Bucket {
    pub fn label(self) -> &'static str {
        match self {
            Bucket::Healthy => "Healthy",
            Bucket::Failing => "Failing",
            Bucket::Held => "Held",
            Bucket::Offline => "Offline",
            Bucket::Drifted => "Drifted",
            Bucket::Unknown => "No reading",
        }
    }

    /// The hosts-table filter the tile links to.
    pub fn filter(self) -> &'static str {
        match self {
            Bucket::Healthy => "healthy",
            Bucket::Failing => "failing",
            Bucket::Held => "held",
            Bucket::Offline => "offline",
            Bucket::Drifted => "drifted",
            Bucket::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub bucket: Bucket,
    pub count: usize,
    /// The newest and oldest readings behind the count: health readings for
    /// the health buckets, the last contact for offline hosts.
    pub newest: Option<DateTime<Utc>>,
    pub oldest: Option<DateTime<Utc>>,
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

/// The roll-up tiles, always in the same order; `Unknown` only appears when a
/// host has no reading. Hosts still awaiting registration are not part of the
/// fleet's health yet and are left out.
pub fn rollup(hosts: &[HostSummary]) -> Vec<Tile> {
    let mut tiles: BTreeMap<Bucket, Tile> = [
        Bucket::Healthy,
        Bucket::Failing,
        Bucket::Held,
        Bucket::Offline,
        Bucket::Drifted,
    ]
    .into_iter()
    .map(|b| {
        (
            b,
            Tile {
                bucket: b,
                count: 0,
                newest: None,
                oldest: None,
            },
        )
    })
    .collect();

    let mut add = |b: Bucket, at: Option<DateTime<Utc>>| {
        let tile = tiles.entry(b).or_insert(Tile {
            bucket: b,
            count: 0,
            newest: None,
            oldest: None,
        });
        tile.count += 1;
        if let Some(at) = at {
            tile.newest = Some(tile.newest.map_or(at, |n| n.max(at)));
            tile.oldest = Some(tile.oldest.map_or(at, |o| o.min(at)));
        }
    };

    for host in hosts.iter().filter(|h| h.status != HostStatus::Pending) {
        let b = bucket(host);
        let at = if b == Bucket::Offline {
            host.last_seen
        } else {
            host.health_at
        };
        add(b, at);
        if host.drifted {
            add(Bucket::Drifted, host.health_at);
        }
    }
    tiles.into_values().collect()
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
}

impl Concern {
    /// How much it asks for: a failure first, a registration last.
    pub fn tone(&self) -> super::tone::Tone {
        use super::tone::Tone;
        match self {
            Concern::Failed { .. } => Tone::Danger,
            Concern::Held { .. }
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

/// The agents needing attention, worst first: by their most severe concern,
/// then by how many they have, then by name.
pub fn attention(
    hosts: &[HostSummary],
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
            first_seen: Utc::now(),
            last_seen: Some(Utc::now()),
            notes: String::new(),
            upgrade: None,
        }
    }

    #[test]
    fn the_rollup_counts_each_host_once_by_health_and_drift_on_top() {
        let now = Utc::now();
        let mut failing = host("b");
        failing.health = HealthSummary::Failing;
        failing.health_at = Some(now - Duration::minutes(3));
        let mut offline = host("c");
        offline.connection = Connection::Offline;
        offline.health = HealthSummary::Failing; // stale reading, not counted as failing
        offline.last_seen = Some(now - Duration::hours(2));
        let mut drifted = host("d");
        drifted.drifted = true;
        drifted.health_at = Some(now - Duration::minutes(14));
        let mut pending = host("e");
        pending.status = HostStatus::Pending;
        let mut healthy = host("a");
        healthy.health_at = Some(now - Duration::minutes(2));

        let tiles = rollup(&[healthy, failing, offline, drifted, pending]);
        let get = |b| tiles.iter().find(|t| t.bucket == b).unwrap().clone();
        assert_eq!(get(Bucket::Healthy).count, 2);
        assert_eq!(get(Bucket::Failing).count, 1);
        assert_eq!(get(Bucket::Offline).count, 1);
        assert_eq!(get(Bucket::Drifted).count, 1);
        assert_eq!(get(Bucket::Held).count, 0);
        assert!(tiles.iter().all(|t| t.bucket != Bucket::Unknown));
        let h = get(Bucket::Healthy);
        assert_eq!(h.newest, Some(now - Duration::minutes(2)));
        assert_eq!(h.oldest, Some(now - Duration::minutes(14)));
        assert_eq!(get(Bucket::Offline).oldest, Some(now - Duration::hours(2)));
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
            }],
            plans: vec![],
            fleet: None,
            events: vec![],
        };

        let hosts = vec![host("pi-2"), host("pi-5"), pending, gone, blip, skewed];
        let list = attention(&hosts, Some("2.0.0"), Some(&rollout), now);
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
}
