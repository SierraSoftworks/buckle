//! The demo fleet: twenty-seven hosts across five stages, one active rollout
//! with a host in every rollout state, a pending registration, a rejected
//! revision, and plans whose secrets appear only as `«secret:NAME@fp»`.
//! Fictional throughout, after the design's reference fleet (§15).

use std::collections::BTreeMap;

use buckle_api::{
    AuditEntry, Catalog, CheckEpoch, Connection, DiffKind, DriftItem, FleetHost, FleetPlan,
    HandlerView, HealthResultDto, HealthSummary, HostCheck, HostDetail, HostRolloutState as S,
    HostStatus, HostSummary, KindDescriptor, Me, ModuleNote, ModuleView, PhaseState, PlanGroup,
    PlanView, RemovalView, ResourceSummary, RevisionStatus, RevisionSummary, RolloutDetail,
    RolloutEvent, RolloutHost, RolloutPhase, RolloutState, RolloutSummary, StepOutcome, StepView,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

pub const SERVER_VERSION: &str = "2.0.0-dev.5";
pub const NEW_REV: &str = "7c2e1f0a9d3b5e7f1c2d4e6f8a0b1c3d5e7f9a1b";
pub const STABLE_REV: &str = "5d4a9b2c7e1f3a5b7c9d1e3f5a7b9c1d3e5f7a9b";
pub const REJECTED_REV: &str = "3b1f8e7c2a4d6f8b0c2e4a6c8e0a2c4e6a8c0e2a";
pub const ROLLED_BACK_REV: &str = "9a0c6d1e3f5b7d9f1b3d5f7b9d1f3b5d7f9b1d3f";
pub const ABORTED_REV: &str = "e8f7a65b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f";
pub const OLD_REV: &str = "1f2e3d4c5b6a7980a1b2c3d4e5f60718293a4b5c";
pub const OLDEST_REV: &str = "0aa1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9";

pub const PLAN_NOMAD: &str = "5b1c9e0244f1a7c3e9b2d8f6a0c4e2b7d9f1a3c5e7b9d1f3a5c7e9b1d3f5a7c9";
pub const PLAN_GREY: &str = "e07aa3419c2b8d4f6a1e3c5b7d9f2a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f";
pub const PLAN_LAB: &str = "a91f3c7e5b2d8f4a6c1e3b5d7f9a2c4e6b8d0f2a4c6e8b0d2f4a6c8e0b2d4f6a";

pub const TRACE: &str = "00af7651c3e94b1a8f0e2d9c4b7a6e15";

/// A trace id that looks like one: 32 hex digits from a seed.
pub fn fake_trace(seed: u64) -> String {
    let a = seed.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let b = a.rotate_left(29).wrapping_mul(0x2545_f491_4f6c_dd1d);
    format!("{a:016x}{b:016x}")
}

pub fn trace_url(id: &str) -> String {
    format!("https://ui.honeycomb.io/example/environments/fleet/trace?trace_id={id}")
}

pub fn me() -> Me {
    Me {
        name: "Alice Operator".into(),
        email: Some("alice@example.com".into()),
        subject: "alice".into(),
        is_admin: true,
        anonymous: false,
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The groups the demo fleet derives from labels: the site, `canary` when
/// asked for, and the role's group.
pub fn groups_for(effective: &BTreeMap<String, String>) -> Vec<String> {
    let mut groups = Vec::new();
    if effective.get("canary").map(String::as_str) == Some("true") {
        groups.push("canary".to_string());
    }
    if let Some(site) = effective.get("site") {
        groups.push(site.clone());
    }
    match effective.get("role").map(String::as_str) {
        Some("worker") => groups.push("workers".into()),
        Some("storage") => groups.push("storage".into()),
        Some("server") => groups.push("servers".into()),
        _ => {}
    }
    groups
}

/// The first phase whose selector matches, as the demo `buckle.yaml` has them.
pub fn phase_for(groups: &[String]) -> Option<String> {
    let has = |g: &str| groups.iter().any(|x| x == g);
    Some(
        if has("canary") {
            "canary"
        } else if has("home") {
            "home"
        } else if has("cloud") {
            "cloud"
        } else {
            "everything-else"
        }
        .to_string(),
    )
}

struct Spec {
    id: String,
    agent: &'static [(&'static str, &'static str)],
    server: &'static [(&'static str, &'static str)],
    platform: &'static str,
    arch: &'static str,
    health: HealthSummary,
    offline_mins: Option<i64>,
    drifted: bool,
    applied: &'static str,
}

const PI: &[(&str, &str)] = &[("role", "worker"), ("site", "home")];
const OCI: &[(&str, &str)] = &[("role", "worker"), ("site", "cloud"), ("provider", "oci")];
const HZ: &[(&str, &str)] = &[
    ("role", "worker"),
    ("site", "cloud"),
    ("provider", "hetzner"),
];
const LAB: &[(&str, &str)] = &[("site", "lab")];

fn specs() -> Vec<Spec> {
    use HealthSummary::*;
    let s = |id: &str, agent, server, platform, arch, health, applied| Spec {
        id: id.to_string(),
        agent,
        server,
        platform,
        arch,
        health,
        offline_mins: None,
        drifted: false,
        applied,
    };
    let mut v = vec![
        s(
            "pi-1",
            PI,
            &[("canary", "true")],
            "debian/bookworm",
            "aarch64",
            Healthy,
            NEW_REV,
        ),
        s(
            "pi-2",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Healthy,
            NEW_REV,
        ),
        s(
            "pi-3",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Healthy,
            NEW_REV,
        ),
        s(
            "pi-4",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Healthy,
            STABLE_REV,
        ),
        s(
            "pi-5",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Failing,
            NEW_REV,
        ),
        s(
            "pi-6",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Healthy,
            STABLE_REV,
        ),
        s(
            "pi-7",
            PI,
            &[],
            "debian/bookworm",
            "aarch64",
            Healthy,
            STABLE_REV,
        ),
        s(
            "nas-1",
            &[("role", "storage"), ("site", "home")],
            &[],
            "debian/bookworm",
            "x86_64",
            Held,
            STABLE_REV,
        ),
    ];
    for i in 1..=6 {
        v.push(s(
            &format!("oci-{i}"),
            OCI,
            &[],
            "ubuntu/24.04",
            "aarch64",
            Healthy,
            STABLE_REV,
        ));
    }
    for i in 1..=6 {
        v.push(s(
            &format!("hz-{i}"),
            HZ,
            &[],
            "debian/bookworm",
            "x86_64",
            Healthy,
            STABLE_REV,
        ));
    }
    v.push(s(
        "az-1",
        &[("role", "worker"), ("site", "cloud"), ("provider", "azure")],
        &[],
        "ubuntu/24.04",
        "x86_64",
        Healthy,
        STABLE_REV,
    ));
    for id in ["lab-1", "lab-2", "lab-3"] {
        v.push(s(
            id,
            LAB,
            &[],
            "debian/trixie",
            "x86_64",
            Healthy,
            STABLE_REV,
        ));
    }
    v.push(s(
        "edge-1",
        &[("site", "edge")],
        &[],
        "debian/bookworm",
        "aarch64",
        Unknown,
        STABLE_REV,
    ));
    v.push(s(
        "lab-4",
        LAB,
        &[],
        "debian/trixie",
        "x86_64",
        Unknown,
        OLD_REV,
    ));
    v.push(s(
        "rpi-new",
        &[("site", "home"), ("role", "worker")],
        &[],
        "debian/bookworm",
        "aarch64",
        Unknown,
        "",
    ));

    for spec in &mut v {
        match spec.id.as_str() {
            "az-1" => spec.offline_mins = Some(25),
            "oci-6" => spec.offline_mins = Some(3),
            "lab-4" => spec.offline_mins = Some(2 * 24 * 60),
            "hz-5" => spec.health = Failing,
            "hz-3" | "lab-2" => spec.drifted = true,
            _ => {}
        }
    }
    v
}

pub fn hosts(now: DateTime<Utc>) -> Vec<HostSummary> {
    specs()
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let agent_labels = labels(s.agent);
            let server_labels = labels(s.server);
            let effective = crate::logic::labels::effective(&agent_labels, &server_labels);
            let groups = groups_for(&effective);
            let pending = s.id == "rpi-new";
            let offline = s.offline_mins.is_some();
            HostSummary {
                id: s.id.clone(),
                status: if pending {
                    HostStatus::Pending
                } else if s.id == "lab-4" {
                    HostStatus::Paused
                } else {
                    HostStatus::Active
                },
                connection: if offline {
                    Connection::Offline
                } else {
                    Connection::Online
                },
                agent_labels,
                labels: server_labels,
                effective_labels: effective,
                groups,
                platform: Some(s.platform.into()),
                arch: Some(s.arch.into()),
                agent_version: Some(
                    if s.id == "lab-3" {
                        "2.0.0-dev.3"
                    } else {
                        SERVER_VERSION
                    }
                    .into(),
                ),
                applied_revision: (!s.applied.is_empty()).then(|| s.applied.into()),
                health: s.health,
                health_at: (!pending).then(|| {
                    now - Duration::minutes(s.offline_mins.unwrap_or(0) + 1 + (i as i64 * 7) % 14)
                }),
                drifted: s.drifted,
                first_seen: now
                    - if s.id == "edge-1" {
                        Duration::minutes(20)
                    } else if pending {
                        Duration::minutes(8)
                    } else {
                        Duration::days(40 + i as i64)
                    },
                last_seen: Some(now - Duration::minutes(s.offline_mins.unwrap_or(0)) - Duration::seconds(i as i64 * 3)),
                notes: match s.id.as_str() {
                    "nas-1" => "Disk replacement scheduled for Thursday; expect the backup marker check to fail until then.".into(),
                    "pi-1" => "Canary. Sits under the desk; reboot by hand if it wedges.".into(),
                    _ => String::new(),
                },
                upgrade: None,
            }
        })
        .collect()
}

fn entry(
    host: &str,
    phase: &str,
    wave: u32,
    state: S,
    digest: Option<&str>,
    now: DateTime<Utc>,
    mins_ago: i64,
) -> RolloutHost {
    RolloutHost {
        host: host.into(),
        phase: phase.into(),
        wave: Some(wave),
        state,
        plan_digest: digest.map(Into::into),
        plan_changes: digest.map(|d| match d {
            PLAN_GREY => 5,
            PLAN_LAB => 2,
            _ => 4,
        }),
        plan_removals: digest.map(|d| if d == PLAN_GREY { 3 } else { 0 }),
        failed_module: None,
        failed_checks: vec![],
        waived_checks: vec![],
        error: None,
        updated_at: now - Duration::minutes(mins_ago),
        bake_until: None,
        needs_approval: false,
        needs_attention: false,
        error_detail: None,
        run_id: None,
    }
}

fn phase(
    name: &str,
    state: PhaseState,
    parallel: &str,
    approval: &str,
    bake: u64,
    max_failures: &str,
    hosts: Vec<RolloutHost>,
) -> RolloutPhase {
    RolloutPhase {
        name: name.into(),
        state,
        parallel: parallel.into(),
        approval: approval.into(),
        bake_seconds: bake,
        max_failures: max_failures.into(),
        virtual_stage: name == "stragglers",
        hosts,
        approved_by: None,
        approved_at: None,
        bake_until: None,
        health_checks: gate_checks(name),
    }
}

/// The demo's health gate per stage: the required checks of the modules
/// the stage's hosts apply.
fn gate_checks(phase: &str) -> Vec<String> {
    let mut checks = vec!["tailscale/up", "docker/daemon", "nomad/api", "nomad/unit"];
    match phase {
        "home" => checks.push("backup/marker"),
        "everything-else" => checks = vec!["tailscale/up", "docker/daemon"],
        "stragglers" => checks = vec![],
        _ => {}
    }
    checks.into_iter().map(Into::into).collect()
}

/// The active rollout: canary done, home in flight with a host in every
/// in-flight state, cloud planned and waiting for approval (one of its two
/// distinct plans removes the retired grey module), the rest pending, and a
/// host enrolled after the freeze waiting in the stragglers stage.
pub fn active_rollout(now: DateTime<Utc>) -> RolloutDetail {
    let mut canary = phase(
        "canary",
        PhaseState::Done,
        "1",
        "required",
        900,
        "0",
        vec![entry(
            "pi-1",
            "canary",
            1,
            S::Healthy,
            Some(PLAN_NOMAD),
            now,
            14,
        )],
    );
    canary.approved_by = Some("alice@example.com".into());
    canary.approved_at = Some(now - Duration::minutes(31));

    let mut pi3 = entry("pi-3", "home", 2, S::Baking, Some(PLAN_NOMAD), now, 2);
    pi3.bake_until = Some(now + Duration::seconds(200));
    let mut pi5 = entry("pi-5", "home", 1, S::Failed, Some(PLAN_NOMAD), now, 4);
    pi5.failed_module = Some("nomad".into());
    pi5.failed_checks = vec!["nomad/api".into()];
    pi5.error = Some("nomad failed at file.template[/etc/nomad.d/nomad.hcl]: nomad config validate rejected the rendered file: client.servers: \"«secret:nomad_server@b21c»\" is not a valid address: missing port in address. Rolling nomad back failed too: apt.package[nomad] is blocked (1.9.2 is no longer in the repository), so the host is left on a mix of revisions; nomad/api has been failing since.".into());
    pi5.error_detail = Some(super::runs::pi5_error());
    pi5.needs_attention = true;
    let mut pi6 = entry("pi-6", "home", 1, S::RolledBack, Some(PLAN_NOMAD), now, 6);
    pi6.error = Some("The agent applied the cached stable revision after 5m without a session while nomad/unit was failing (local rollback).".into());
    pi6.error_detail = Some(super::runs::local_rollback_error());
    let mut pi7 = entry("pi-7", "home", 2, S::Skipped, Some(PLAN_NOMAD), now, 9);
    pi7.error = Some("Skipped by bob@example.com: board swap in progress.".into());
    let mut nas = entry("nas-1", "home", 2, S::Held, Some(PLAN_NOMAD), now, 3);
    nas.error = Some("Held at baseline: backup/marker failing (/var/lib/rustic/last-snapshot is 26h old, max_age 25h).".into());
    nas.failed_checks = vec!["backup/marker".into()];
    let home = phase(
        "home",
        PhaseState::Running,
        "2",
        "auto",
        300,
        "1",
        vec![
            entry("pi-2", "home", 1, S::Healthy, Some(PLAN_NOMAD), now, 7),
            pi3,
            entry("pi-4", "home", 2, S::Applying, Some(PLAN_NOMAD), now, 1),
            pi5,
            pi6,
            pi7,
            nas,
        ],
    );

    let mut cloud_hosts = Vec::new();
    for i in 1..=6 {
        cloud_hosts.push(entry(
            &format!("oci-{i}"),
            "cloud",
            if i <= 3 { 1 } else { 2 },
            S::Planned,
            Some(PLAN_NOMAD),
            now,
            30,
        ));
    }
    for i in 1..=6 {
        let digest = if i <= 2 { PLAN_NOMAD } else { PLAN_GREY };
        cloud_hosts.push(entry(
            &format!("hz-{i}"),
            "cloud",
            if i <= 3 { 1 } else { 2 },
            S::Planned,
            Some(digest),
            now,
            29,
        ));
    }
    for h in &mut cloud_hosts {
        h.needs_approval = true;
    }
    let mut az = entry("az-1", "cloud", 2, S::PlanPending, None, now, 25);
    az.error =
        Some("No plan yet: the agent has been offline since before the rollout started.".into());
    cloud_hosts.push(az);
    let cloud = phase(
        "cloud",
        PhaseState::AwaitingApproval,
        "50%",
        "if_destructive",
        300,
        "10%",
        cloud_hosts,
    );

    let rest = phase(
        "everything-else",
        PhaseState::Pending,
        "2",
        "if_destructive",
        300,
        "0",
        vec![
            entry(
                "lab-1",
                "everything-else",
                1,
                S::Planned,
                Some(PLAN_LAB),
                now,
                30,
            ),
            entry(
                "lab-2",
                "everything-else",
                1,
                S::Planned,
                Some(PLAN_LAB),
                now,
                30,
            ),
            entry("lab-3", "everything-else", 1, S::Pending, None, now, 30),
        ],
    );
    let stragglers = phase(
        "stragglers",
        PhaseState::Pending,
        "2",
        "auto",
        300,
        "0",
        vec![{
            let mut e = entry("edge-1", "stragglers", 1, S::Pending, None, now, 20);
            e.wave = None;
            e.error = Some("Enrolled after the target set was frozen; converges once the revision is promoted.".into());
            e
        }],
    );

    let mut phases = vec![canary, home, cloud, rest, stragglers];
    // The hosts that have run this rollout's revision.
    for h in phases.iter_mut().flat_map(|p| p.hosts.iter_mut()) {
        if matches!(
            h.state,
            S::Healthy | S::Baking | S::Applying | S::Failed | S::RolledBack
        ) {
            h.run_id = Some(super::runs::run_id(&h.host, NEW_REV));
        }
    }
    let total = phases.iter().map(|p| p.hosts.len() as u32).sum();
    let events = timeline(now);

    RolloutDetail {
        summary: RolloutSummary {
            id: "42".into(),
            name: None,
            revision: NEW_REV.into(),
            revision_message: "nomad 1.9.3, rotate nomad_server".into(),
            state: RolloutState::Rolling,
            started_at: now - Duration::minutes(34),
            finished_at: None,
            triggered_by: "controller (trigger: auto)".into(),
            hosts_total: total,
            hosts_done: 2,
            hosts_failed: 1,
            current_phase: Some("home".into()),
            pause_reason: None,
            trace_id: Some(TRACE.into()),
            trace_url: Some(trace_url(TRACE)),
        },
        phases,
        plans: plans(),
        fleet: Some(fleet_plan()),
        events,
    }
}

/// `?demo&awaiting`: the home phase has finished, so the rollout is held at
/// the cloud gate waiting for an approval (and can be rejected).
pub fn hold_at_cloud_gate(detail: &mut RolloutDetail, now: DateTime<Utc>) {
    for phase in &mut detail.phases {
        if phase.name == "home" {
            phase.state = PhaseState::Done;
            for h in &mut phase.hosts {
                if matches!(h.state, S::Baking | S::Applying | S::Held) {
                    h.state = S::Healthy;
                    h.bake_until = None;
                    h.error = None;
                    h.failed_checks.clear();
                }
            }
        }
    }
    detail.summary.state = RolloutState::AwaitingApproval;
    detail.summary.current_phase = Some("cloud".into());
    detail.summary.hosts_done = detail
        .phases
        .iter()
        .flat_map(|p| &p.hosts)
        .filter(|h| h.state == S::Healthy)
        .count() as u32;
    detail.events.push(ev(
        now,
        0,
        "rollout.phase.done",
        "phase 'home' done; pre-phase gate passed",
        None,
        Some("home"),
        Some("controller"),
    ));
}

fn ev(
    now: DateTime<Utc>,
    mins_ago: i64,
    kind: &str,
    message: &str,
    host: Option<&str>,
    phase: Option<&str>,
    actor: Option<&str>,
) -> RolloutEvent {
    RolloutEvent {
        at: now - Duration::seconds(mins_ago * 60),
        kind: kind.into(),
        message: message.into(),
        host: host.map(Into::into),
        phase: phase.map(Into::into),
        actor: actor.map(Into::into),
    }
}

fn timeline(now: DateTime<Utc>) -> Vec<RolloutEvent> {
    let mut v = vec![
        ev(
            now,
            34,
            "rollout.started",
            "Revision 7c2e1f0 detected; rollout started (trigger: auto).",
            None,
            None,
            Some("controller"),
        ),
        ev(
            now,
            33,
            "rollout.planned",
            "Plans requested from 26 hosts; 3 distinct digests so far.",
            None,
            None,
            Some("controller"),
        ),
        ev(
            now,
            31,
            "phase.approved",
            "Approved plan 5b1c9e02 for 1 host.",
            None,
            Some("canary"),
            Some("alice@example.com"),
        ),
        ev(
            now,
            30,
            "host.baseline",
            "Baseline checks passed (nomad/api, nomad/unit, tailscale/status).",
            Some("pi-1"),
            Some("canary"),
            None,
        ),
        ev(
            now,
            29,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-1"),
            Some("canary"),
            None,
        ),
        ev(
            now,
            29,
            "host.applied",
            "Applied 4 changes in 41s; handler nomad:restart ran.",
            Some("pi-1"),
            Some("canary"),
            None,
        ),
        ev(
            now,
            28,
            "host.bake_started",
            "Baking for 15m.",
            Some("pi-1"),
            Some("canary"),
            None,
        ),
        ev(
            now,
            14,
            "host.healthy",
            "Bake window passed without a required check failing.",
            Some("pi-1"),
            Some("canary"),
            None,
        ),
        ev(
            now,
            13,
            "phase.done",
            "Pre-phase check passed on 1 host; advancing to home.",
            None,
            Some("canary"),
            Some("controller"),
        ),
        ev(
            now,
            12,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-2"),
            Some("home"),
            None,
        ),
        ev(
            now,
            12,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-5"),
            Some("home"),
            None,
        ),
        ev(
            now,
            12,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-6"),
            Some("home"),
            None,
        ),
        ev(
            now,
            11,
            "host.applied",
            "Applied 4 changes in 38s.",
            Some("pi-2"),
            Some("home"),
            None,
        ),
        ev(
            now,
            11,
            "host.applied",
            "Applied 4 changes in 52s.",
            Some("pi-5"),
            Some("home"),
            None,
        ),
        ev(
            now,
            11,
            "host.session_lost",
            "Session lost 4s after apply.",
            Some("pi-6"),
            Some("home"),
            None,
        ),
        ev(
            now,
            9,
            "host.skipped",
            "Skipped: board swap in progress.",
            Some("pi-7"),
            Some("home"),
            Some("bob@example.com"),
        ),
        ev(
            now,
            8,
            "host.check_failing",
            "nomad/api failing: GET /v1/agent/health answered 500 (settle 2m running).",
            Some("pi-5"),
            Some("home"),
            None,
        ),
        ev(
            now,
            7,
            "host.healthy",
            "Bake window passed without a required check failing.",
            Some("pi-2"),
            Some("home"),
            None,
        ),
        ev(
            now,
            6,
            "host.local_rollback",
            "Reconnected after a local rollback to 5d4a9b2; evidence attached.",
            Some("pi-6"),
            Some("home"),
            None,
        ),
        ev(
            now,
            6,
            "host.failed",
            "nomad/api failed past settle; counts against max_failures (1 of 1).",
            Some("pi-5"),
            Some("home"),
            Some("controller"),
        ),
        ev(
            now,
            5,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-3"),
            Some("home"),
            None,
        ),
        ev(
            now,
            4,
            "host.applied",
            "Applied 4 changes in 44s.",
            Some("pi-3"),
            Some("home"),
            None,
        ),
        ev(
            now,
            3,
            "host.held",
            "Baseline check backup/marker failing; held for an operator.",
            Some("nas-1"),
            Some("home"),
            Some("controller"),
        ),
        ev(
            now,
            2,
            "host.bake_started",
            "Baking for 5m.",
            Some("pi-3"),
            Some("home"),
            None,
        ),
        ev(
            now,
            1,
            "host.apply_started",
            "Applying plan 5b1c9e02 (4 changes).",
            Some("pi-4"),
            Some("home"),
            None,
        ),
    ];
    v.sort_by_key(|e| e.at);
    v
}

/// A step of the demo plans: `diff` with its summary for a change, `None`
/// for a step that is already in line.
fn step(module: &str, kind: &str, key: &str, diff: Option<(DiffKind, &str)>) -> StepView {
    StepView {
        kind: kind.into(),
        key: key.into(),
        task: key.into(),
        name: None,
        outcome: if diff.is_some() {
            StepOutcome::Change
        } else {
            StepOutcome::NoOp
        },
        summary: diff.map(|(_, s)| s.to_string()).unwrap_or_default(),
        diff: diff.map(|(d, _)| d),
        unified: None,
        preview: None,
        destructive: matches!(diff, Some((DiffKind::Delete, _))),
        locks: if kind.starts_with("apt.") {
            vec!["apt".into()]
        } else {
            vec![]
        },
        notifies: if module == "nomad" && kind != "systemd.unit" {
            vec!["nomad:restart".into()]
        } else {
            vec![]
        },
        always_runs: false,
    }
}

fn with_diff(mut step: StepView, unified: &str) -> StepView {
    step.unified = Some(unified.into());
    step
}

const NOMAD_DIFF: &str = "--- current\n+++ planned\n@@ -3,7 +3,7 @@\n datacenter = \"home\"\n data_dir   = \"/opt/nomad/data\"\n \n client {\n   enabled = true\n-  servers = [\"«secret:nomad_server@3f9a»\"]\n+  servers = [\"«secret:nomad_server@b21c»\"]\n }\n@@ -18,3 +18,4 @@\n telemetry {\n   publish_allocation_metrics = true\n+  prometheus_metrics         = true\n }\n";

fn common_module() -> ModuleView {
    ModuleView {
        module: "common".into(),
        needs: vec![],
        steps: vec![
            step("common", "apt.package", "ufw, jq, curl", None),
            step(
                "common",
                "file.line",
                "/etc/ssh/sshd_config:PasswordAuthentication",
                Some((DiffKind::Update, "append line PasswordAuthentication no")),
            ),
            with_diff(
                step(
                    "common",
                    "sysctl.param",
                    "vm.swappiness",
                    Some((DiffKind::Update, "1 → 1 (drift: current 60)")),
                ),
                "--- current\n+++ planned\n@@ -1 +1 @@\n-vm.swappiness = 60\n+vm.swappiness = 1\n",
            ),
        ],
        handlers: vec![],
        deferred: false,
    }
}

fn removal(
    kind: &str,
    key: &str,
    summary: &str,
    cleanup: bool,
    caution: bool,
    introduced: Option<&str>,
) -> RemovalView {
    RemovalView {
        module: "grey".into(),
        kind: kind.into(),
        key: key.into(),
        summary: summary.into(),
        cleanup,
        caution,
        introduced_in: introduced.map(Into::into),
    }
}

fn plan_doc(host: &str, digest: &str, grey: bool, nomad: bool) -> PlanView {
    // Already in place everywhere: a module the plan review leaves out.
    let tailscale = ModuleView {
        module: "tailscale".into(),
        needs: vec![],
        steps: vec![
            step("tailscale", "apt.package", "tailscale", None),
            step("tailscale", "systemd.unit", "tailscaled", None),
        ],
        handlers: vec![],
        deferred: false,
    };
    let mut modules = vec![common_module(), tailscale];
    if nomad {
        modules.push(ModuleView {
            module: "nomad".into(),
            needs: vec!["tailscale".into(), "docker".into()],
            steps: vec![
                step(
                    "nomad",
                    "apt.package",
                    "nomad",
                    Some((DiffKind::Update, "1.9.2 → 1.9.3")),
                ),
                with_diff(
                    step(
                        "nomad",
                        "file.template",
                        "/etc/nomad.d/nomad.hcl",
                        Some((DiffKind::Update, "3 lines changed")),
                    ),
                    NOMAD_DIFF,
                ),
                with_diff(
                    step(
                        "nomad",
                        "file.template",
                        "/etc/nomad.d/client.hcl",
                        Some((DiffKind::Update, "1 line changed")),
                    ),
                    &format!(
                        "--- current\n+++ planned\n@@ -1,3 +1,3 @@\n name = \"{host}\"\n-advertise {{ http = \"0.0.0.0\" }}\n+advertise {{ http = \"{}\" }}\n datacenter = \"home\"\n",
                        tailnet_ip(host)
                    ),
                ),
                step("nomad", "systemd.unit", "nomad", None),
            ],
            handlers: vec![HandlerView {
                handler: "restart".into(),
                kind: "systemd.restart".into(),
                summary: "restart nomad.service".into(),
            }],
            deferred: false,
        });
    }
    if grey {
        modules.push(ModuleView {
            module: "cleanup".into(),
            needs: vec![],
            steps: vec![step(
                "cleanup",
                "file.absent",
                "/etc/grey/agent.token",
                Some((
                    DiffKind::Delete,
                    "delete /etc/grey/agent.token (contains «secret:grey_token@91d0»)",
                )),
            )],
            handlers: vec![],
            deferred: false,
        });
    }
    let removals = if grey {
        vec![
            removal(
                "file.template",
                "/var/www/grey/config.yaml",
                "module no longer selected · delete file (content still matches what Buckle wrote)",
                true,
                false,
                Some(OLD_REV),
            ),
            removal(
                "systemd.unit",
                "grey",
                "disable + stop grey.service",
                true,
                false,
                Some(OLD_REV),
            ),
            removal(
                "apt.package",
                "grey-agent",
                "remove package grey-agent (apt purge)",
                true,
                true,
                Some(OLDEST_REV),
            ),
            removal(
                "file.copy",
                "/var/log/grey",
                "forget only (removal: leave)",
                false,
                false,
                None,
            ),
        ]
    } else {
        vec![]
    };
    PlanView {
        revision: NEW_REV.into(),
        host: host.into(),
        digest: digest.into(),
        modules,
        removals,
        rehomed: vec![],
        deferred: if nomad {
            vec![ModuleNote {
                module: "otel_ebpf_profiler".into(),
                reason: "waits for facts from docker (docker.version)".into(),
                waiting_on: vec!["docker.version".into()],
                plan_error: false,
            }]
        } else {
            vec![]
        },
        held: vec![],
        indeterminate: vec![],
        skipped: vec![
            "backup: host.groups contains \"storage\"".into(),
            "caddy: host.labels.role == \"edge\"".into(),
        ],
        warnings: if grey {
            vec![
                "Removing apt.package[grey-agent] can remove packages that depend on it.".into(),
                format!(
                    "removal: grey/command.run[/usr/local/bin/grey-agent deregister --node {host} && systemd-run --unit=grey-agent-cleanup --on-active=1min --timer-property=AccuracySec=1s /usr/bin/true] — released: left in place (policy leave)"
                ),
            ]
        } else {
            vec![]
        },
    }
}

/// A host's (made up) tailnet address.
fn tailnet_ip(host: &str) -> String {
    let n = host
        .bytes()
        .fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    format!("100.64.{}.{}", n % 7, 10 + n % 200)
}

/// The demo rollout's plans as one union, each host planning its own copy
/// (so the per-host template above differs only by fact values).
pub fn fleet_plan() -> FleetPlan {
    let hosts: Vec<FleetHost> = plans()
        .iter()
        .flat_map(|g| {
            let plan = g.plan.clone().unwrap_or_default();
            let has = |m: &str| plan.modules.iter().any(|x| x.module == m);
            let (grey, nomad) = (has("cleanup"), has("nomad"));
            g.hosts.iter().map(move |h| FleetHost {
                host: h.clone(),
                plan: Some(plan_doc(h, &g.digest, grey, nomad)),
                context: json!({
                    "id": h,
                    "labels": {},
                    "facts": {"tailscale": {"ip": tailnet_ip(h)}, "os": {"codename": "bookworm"}},
                }),
            })
        })
        .collect();
    FleetPlan::build(&hosts)
}

pub fn plans() -> Vec<PlanGroup> {
    let mut nomad_hosts: Vec<String> = [
        "pi-1", "pi-2", "pi-3", "pi-4", "pi-5", "pi-6", "pi-7", "nas-1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    nomad_hosts.extend((1..=6).map(|i| format!("oci-{i}")));
    nomad_hosts.extend(["hz-1".into(), "hz-2".into()]);
    vec![
        PlanGroup {
            digest: PLAN_NOMAD.into(),
            hosts: nomad_hosts,
            changes: 4,
            removals: 0,
            destructive: false,
            plan: Some(plan_doc("pi-1", PLAN_NOMAD, false, true)),
        },
        PlanGroup {
            digest: PLAN_GREY.into(),
            hosts: (3..=6).map(|i| format!("hz-{i}")).collect(),
            changes: 5,
            removals: 3,
            destructive: true,
            plan: Some(plan_doc("hz-3", PLAN_GREY, true, true)),
        },
        PlanGroup {
            digest: PLAN_LAB.into(),
            hosts: vec!["lab-1".into(), "lab-2".into()],
            changes: 2,
            removals: 0,
            destructive: false,
            plan: Some(plan_doc("lab-1", PLAN_LAB, false, false)),
        },
    ]
}

/// A finished rollout: every host in its final state.
fn finished(
    id: &str,
    rev: &str,
    message: &str,
    state: RolloutState,
    days_ago: i64,
    now: DateTime<Utc>,
) -> RolloutDetail {
    let started = now - Duration::days(days_ago);
    let final_state = match state {
        RolloutState::Completed => S::Healthy,
        RolloutState::RolledBack => S::RolledBack,
        _ => S::Skipped,
    };
    let at = |mins: i64| started + Duration::minutes(mins);
    let mk = |host: &str, phase: &str, s: S, mins: i64| RolloutHost {
        updated_at: at(mins),
        ..entry(host, phase, 1, s, Some(PLAN_NOMAD), now, 0)
    };
    let mut canary_host = mk(
        "pi-1",
        "canary",
        if state == RolloutState::Completed {
            S::Healthy
        } else {
            S::Failed
        },
        20,
    );
    if state != RolloutState::Completed {
        canary_host.failed_module = Some(
            if state == RolloutState::RolledBack {
                "docker"
            } else {
                "swap"
            }
            .into(),
        );
        canary_host.failed_checks = vec![
            if state == RolloutState::RolledBack {
                "docker/daemon"
            } else {
                "swap/zram"
            }
            .into(),
        ];
        if state == RolloutState::RolledBack {
            canary_host.state = S::RolledBack;
        }
    }
    let mut canary = phase(
        "canary",
        PhaseState::Done,
        "1",
        "required",
        900,
        "0",
        vec![canary_host],
    );
    canary.approved_by = Some("alice@example.com".into());
    canary.approved_at = Some(at(2));
    let rest_state = if state == RolloutState::Completed {
        PhaseState::Done
    } else {
        PhaseState::Skipped
    };
    let home = phase(
        "home",
        rest_state,
        "2",
        "auto",
        300,
        "1",
        ["pi-2", "pi-3", "pi-4"]
            .iter()
            .map(|h| mk(h, "home", final_state, 40))
            .collect(),
    );
    let cloud = phase(
        "cloud",
        rest_state,
        "50%",
        "if_destructive",
        300,
        "10%",
        (1..=4)
            .map(|i| mk(&format!("oci-{i}"), "cloud", final_state, 60))
            .collect(),
    );
    let phases = vec![canary, home, cloud];
    let total: u32 = phases.iter().map(|p| p.hosts.len() as u32).sum();
    let mut events = vec![RolloutEvent {
        at: started,
        kind: "rollout.started".into(),
        message: format!("Revision {} detected; rollout started.", &rev[..7]),
        host: None,
        phase: None,
        actor: Some("controller".into()),
    }];
    match state {
        RolloutState::RolledBack => {
            events.push(RolloutEvent {
                at: at(18),
                kind: "rollout.paused".into(),
                message: "pi-1 failed docker/daemon; max_failures 0 breached.".into(),
                host: Some("pi-1".into()),
                phase: Some("canary".into()),
                actor: Some("controller".into()),
            });
            events.push(RolloutEvent {
                at: at(25),
                kind: "rollout.rollback".into(),
                message: "Rolling back 1 host to 0aa1b2c.".into(),
                host: None,
                phase: None,
                actor: Some("alice@example.com".into()),
            });
        }
        RolloutState::Aborted => {
            events.push(RolloutEvent {
                at: at(18),
                kind: "rollout.paused".into(),
                message: "pi-1 failed swap/zram.".into(),
                host: Some("pi-1".into()),
                phase: Some("canary".into()),
                actor: Some("controller".into()),
            });
            events.push(RolloutEvent {
                at: at(30),
                kind: "rollout.aborted".into(),
                message: "Aborted: zram needs a kernel module the Pis do not ship.".into(),
                host: None,
                phase: None,
                actor: Some("bob@example.com".into()),
            });
        }
        _ => events.push(RolloutEvent {
            at: at(75),
            kind: "rollout.completed".into(),
            message: "Last phase baked; revision promoted to stable.".into(),
            host: None,
            phase: None,
            actor: Some("controller".into()),
        }),
    }
    RolloutDetail {
        summary: RolloutSummary {
            id: id.into(),
            name: None,
            revision: rev.into(),
            revision_message: message.into(),
            state,
            started_at: started,
            finished_at: Some(at(if state == RolloutState::Completed {
                75
            } else {
                30
            })),
            triggered_by: if state == RolloutState::Completed {
                "controller (trigger: auto)"
            } else {
                "alice@example.com"
            }
            .into(),
            hosts_total: total,
            hosts_done: if state == RolloutState::Completed {
                total
            } else {
                0
            },
            hosts_failed: if state == RolloutState::Completed {
                0
            } else {
                1
            },
            current_phase: None,
            pause_reason: match state {
                RolloutState::RolledBack => {
                    Some("pi-1 failed docker/daemon in canary (max_failures: 0).".into())
                }
                RolloutState::Aborted => Some("pi-1 failed swap/zram in canary.".into()),
                _ => None,
            },
            trace_id: Some(fake_trace(id.parse().unwrap_or(0))),
            trace_url: Some(trace_url(&fake_trace(id.parse().unwrap_or(0)))),
        },
        phases,
        fleet: None,
        plans: vec![PlanGroup {
            digest: PLAN_NOMAD.into(),
            hosts: vec!["pi-1".into()],
            changes: 2,
            removals: 0,
            destructive: false,
            plan: Some(plan_doc("pi-1", PLAN_NOMAD, false, false)),
        }],
        events,
    }
}

pub fn rollouts(now: DateTime<Utc>) -> Vec<RolloutDetail> {
    vec![
        active_rollout(now),
        finished(
            "41",
            STABLE_REV,
            "caddy: add the grey upstream",
            RolloutState::Completed,
            2,
            now,
        ),
        finished(
            "40",
            ROLLED_BACK_REV,
            "docker 27 → 28",
            RolloutState::RolledBack,
            5,
            now,
        ),
        finished(
            "39",
            ABORTED_REV,
            "experimental: zram swap on the Pis",
            RolloutState::Aborted,
            9,
            now,
        ),
    ]
}

pub fn revisions(now: DateTime<Utc>) -> Vec<RevisionSummary> {
    let r = |id: &str,
             author: &str,
             message: &str,
             ago: Duration,
             status: RevisionStatus,
             rollout: Option<&str>,
             stable: bool| RevisionSummary {
        id: id.into(),
        commit: id.into(),
        author: author.into(),
        message: message.into(),
        created_at: now - ago,
        status,
        validation_errors: vec![],
        is_stable: stable,
        rollout_id: rollout.map(Into::into),
        module_count: 10,
    };
    let mut rejected = r(
        REJECTED_REV,
        "Bob Builder <bob@example.com>",
        "tailscale: try the new ACL tags\n\nSwitches every node to tag:fleet.",
        Duration::hours(3),
        RevisionStatus::Rejected,
        None,
        false,
    );
    rejected.validation_errors = vec![
        "modules/tailscale/module.yaml:14:9: unknown field `tag`, expected one of `tags`, `accept_routes`, `advertise_routes`.".into(),
        "modules/tailscale/module.yaml: health check `tailscale/status` refers to fact `tailscale.backend_state`, which no fact provider registers.".into(),
    ];
    vec![
        r(
            NEW_REV,
            "Alice Operator <alice@example.com>",
            "nomad 1.9.3, rotate nomad_server\n\nThe old server token leaked into a support bundle.",
            Duration::minutes(36),
            RevisionStatus::Valid,
            Some("42"),
            false,
        ),
        rejected,
        r(
            STABLE_REV,
            "Alice Operator <alice@example.com>",
            "caddy: add the grey upstream",
            Duration::days(2) + Duration::minutes(5),
            RevisionStatus::Valid,
            Some("41"),
            true,
        ),
        r(
            ROLLED_BACK_REV,
            "Bob Builder <bob@example.com>",
            "docker 27 → 28",
            Duration::days(5) + Duration::minutes(5),
            RevisionStatus::Valid,
            Some("40"),
            false,
        ),
        r(
            ABORTED_REV,
            "Bob Builder <bob@example.com>",
            "experimental: zram swap on the Pis",
            Duration::days(9) + Duration::minutes(5),
            RevisionStatus::Valid,
            Some("39"),
            false,
        ),
        r(
            OLD_REV,
            "Alice Operator <alice@example.com>",
            "grey: retire the uptime module",
            Duration::days(12),
            RevisionStatus::Valid,
            None,
            false,
        ),
        r(
            OLDEST_REV,
            "Alice Operator <alice@example.com>",
            "initial fleet configuration",
            Duration::days(20),
            RevisionStatus::Valid,
            None,
            false,
        ),
    ]
}

pub fn audit(now: DateTime<Utc>) -> Vec<AuditEntry> {
    let e = |id: u64,
             mins: i64,
             actor: &str,
             action: &str,
             outcome: &str,
             subject: Option<&str>,
             message: &str,
             detail: Option<Value>| AuditEntry {
        id,
        at: now - Duration::minutes(mins),
        actor: actor.into(),
        action: action.into(),
        outcome: outcome.into(),
        subject: subject.map(Into::into),
        message: message.into(),
        detail,
        trace_id: (!id.is_multiple_of(3)).then(|| fake_trace(id)),
    };
    let mut v = vec![
        e(
            1,
            20 * 1440,
            "controller",
            "revision.valid",
            "success",
            Some(&OLDEST_REV[..7]),
            "Revision 0aa1b2c validated (10 modules).",
            None,
        ),
        e(
            2,
            9 * 1440,
            "alice",
            "rollout.start",
            "success",
            Some("39"),
            "alice started rollout 39 of e8f7a65.",
            Some(json!({"revision": ABORTED_REV})),
        ),
        e(
            3,
            9 * 1440 - 30,
            "bob",
            "rollout.abort",
            "success",
            Some("39"),
            "bob aborted rollout 39: zram needs a kernel module the Pis do not ship.",
            None,
        ),
        e(
            4,
            5 * 1440 - 18,
            "controller",
            "rollout.pause",
            "success",
            Some("40"),
            "Rollout 40 paused: pi-1 failed docker/daemon (max_failures 0).",
            Some(json!({"phase": "canary", "host": "pi-1"})),
        ),
        e(
            5,
            5 * 1440 - 25,
            "alice",
            "rollout.rollback",
            "success",
            Some("40"),
            "alice rolled back rollout 40.",
            None,
        ),
        e(
            6,
            2 * 1440 - 75,
            "controller",
            "rollout.complete",
            "success",
            Some("41"),
            "Rollout 41 completed; 5d4a9b2 is stable.",
            None,
        ),
        e(
            7,
            180,
            "controller",
            "revision.rejected",
            "failure",
            Some(&REJECTED_REV[..7]),
            "Revision 3b1f8e7 rejected: 2 validation errors.",
            Some(json!({"errors": 2})),
        ),
        e(
            8,
            60,
            "carol",
            "rollout.approve",
            "denied",
            Some("41"),
            "carol may not approve: admin_acl refused.",
            Some(json!({"phase": "cloud"})),
        ),
        e(
            9,
            34,
            "controller",
            "rollout.start",
            "success",
            Some("42"),
            "Rollout 42 started for 7c2e1f0 (trigger: auto).",
            Some(json!({"revision": NEW_REV})),
        ),
        e(
            10,
            31,
            "alice",
            "rollout.approve",
            "success",
            Some("42"),
            "alice approved canary of rollout 42 (1 plan, 1 host).",
            Some(json!({"phase": "canary", "plan_digests": [PLAN_NOMAD], "hosts": 1})),
        ),
        e(
            11,
            13,
            "controller",
            "phase.advance",
            "success",
            Some("42"),
            "Rollout 42: canary baked; advancing to home.",
            Some(json!({"phase": "home"})),
        ),
        e(
            12,
            9,
            "bob",
            "host.skip",
            "success",
            Some("pi-7"),
            "bob skipped pi-7 in rollout 42: board swap in progress.",
            Some(json!({"rollout": "42"})),
        ),
        e(
            13,
            8,
            "system",
            "host.enrol",
            "success",
            Some("rpi-new"),
            "rpi-new enrolled with a token; awaiting approval.",
            Some(json!({"labels": {"site": "home", "role": "worker"}})),
        ),
        e(
            14,
            6,
            "controller",
            "host.failed",
            "failure",
            Some("pi-5"),
            "pi-5 failed nomad/api in rollout 42 (1 of 1 allowed).",
            Some(json!({"rollout": "42", "phase": "home", "checks": ["nomad/api"]})),
        ),
        e(
            15,
            6,
            "controller",
            "host.local_rollback",
            "failure",
            Some("pi-6"),
            "pi-6 rolled itself back to 5d4a9b2 while disconnected.",
            Some(json!({"rollout": "42"})),
        ),
        e(
            16,
            4,
            "alice",
            "host.update",
            "success",
            Some("pi-1"),
            "alice updated pi-1: labels canary=true.",
            Some(json!({"labels": {"canary": "true"}})),
        ),
        e(
            17,
            3,
            "controller",
            "host.held",
            "success",
            Some("nas-1"),
            "nas-1 held at baseline: backup/marker failing.",
            Some(json!({"rollout": "42", "checks": ["backup/marker"]})),
        ),
        e(
            18,
            2,
            "alice",
            "host.pause",
            "success",
            Some("lab-4"),
            "alice paused lab-4.",
            None,
        ),
        // Progress transitions, hidden on the audit page until asked for.
        e(
            19,
            2,
            "controller",
            "rollout.host.baking",
            "ok",
            Some("42"),
            "pi-3 is baking for 5m.",
            Some(json!({"host": "pi-3", "phase": "home"})),
        ),
        e(
            20,
            1,
            "controller",
            "rollout.host.applying",
            "ok",
            Some("42"),
            "pi-4 is applying plan 5b1c9e02.",
            Some(json!({"host": "pi-4", "phase": "home"})),
        ),
    ];
    v.reverse();
    v
}

fn facts(host: &HostSummary) -> Value {
    let pi = host.id.starts_with("pi") || host.id.starts_with("rpi");
    json!({
        "os": {
            "family": "debian",
            "distribution": host.platform.as_deref().unwrap_or("debian/bookworm").split('/').next(),
            "version": host.platform.as_deref().unwrap_or("debian/bookworm").split('/').nth(1),
            "kernel": if pi { "6.6.51+rpt-rpi-v8" } else { "6.8.0-48-generic" },
            "arch": host.arch,
        },
        "hardware": {
            "cpus": if pi { 4 } else { 8 },
            "memory_mb": if pi { 8_192 } else { 32_768 },
            "model": if pi { "Raspberry Pi 5 Model B Rev 1.0" } else { "KVM virtual machine" },
        },
        "disk": {"root": {"free_percent": if host.id == "nas-1" { 7.4 } else { 61.2 }, "size_gb": if pi { 128 } else { 200 }}},
        "net": {
            "hostname": host.id,
            "fqdn": format!("{}.fleet.example", host.id),
            "interfaces": {
                "eth0": {"ipv4": ["192.168.1.21"], "mac": "dc:a6:32:00:00:01", "up": true},
                "tailscale0": {"ipv4": ["100.64.0.21"], "up": true}
            }
        },
        "tailscale": {"version": "1.76.6", "online": true, "tags": ["tag:fleet"], "ip": "100.64.0.21"},
        "systemd": {"version": 252, "failed_units": if host.health == HealthSummary::Failing { json!(["nomad.service"]) } else { json!([]) }},
        "docker": {"version": "27.3.1", "running": 6},
        "nomad": {"version": "1.9.2", "node_class": "worker", "eligible": true}
    })
}

pub fn host_detail(host: &HostSummary, now: DateTime<Utc>) -> HostDetail {
    let pending = host.status == HostStatus::Pending;
    let rev = host.applied_revision.clone().unwrap_or_default();
    let resources = if pending {
        vec![]
    } else {
        [
            ("common", "apt.package", "ufw, jq, curl", "cleanup"),
            (
                "common",
                "file.line",
                "/etc/ssh/sshd_config:PasswordAuthentication",
                "cleanup",
            ),
            ("common", "sysctl.param", "vm.swappiness", "leave"),
            ("tailscale", "apt.package", "tailscale", "cleanup"),
            ("nomad", "apt.package", "nomad", "cleanup"),
            (
                "nomad",
                "file.template",
                "/etc/nomad.d/nomad.hcl",
                "cleanup",
            ),
            ("nomad", "systemd.unit", "nomad", "cleanup"),
            ("docker", "apt.package", "docker-ce", "cleanup"),
        ]
        .iter()
        .map(|(m, k, key, removal)| ResourceSummary {
            module: m.to_string(),
            kind: k.to_string(),
            key: key.to_string(),
            revision: rev.clone(),
            applied_at: now - Duration::days(2),
            removal: removal.to_string(),
        })
        .collect()
    };
    let check = |scope: &str,
                 module: &str,
                 check: &str,
                 kind: &str,
                 status: &str,
                 reason: Option<&str>,
                 mins: i64,
                 rollout: Option<&str>| HealthResultDto {
        scope: scope.into(),
        module: module.into(),
        check: check.into(),
        kind: kind.into(),
        required: check != "metrics",
        status: status.into(),
        reason: reason.map(Into::into),
        at: now - Duration::minutes(mins),
        rollout_id: rollout.map(Into::into),
    };
    // The drift readings are the ones the host's health summary was taken from.
    let read = host
        .health_at
        .map(|at| (now - at).num_minutes())
        .unwrap_or(3);
    let failing = host.health == HealthSummary::Failing;
    let held = host.health == HealthSummary::Held;
    let mut health_results = if pending {
        vec![]
    } else {
        vec![
            check(
                "drift",
                "nomad",
                "api",
                "http",
                if failing { "fail" } else { "pass" },
                failing.then_some("GET http://127.0.0.1:4646/v1/agent/health answered 500"),
                read,
                None,
            ),
            check(
                "drift", "nomad", "unit", "systemd", "pass", None, read, None,
            ),
            check(
                "drift",
                "tailscale",
                "status",
                "command",
                "pass",
                None,
                read,
                None,
            ),
            check(
                "drift",
                "backup",
                "marker",
                "file",
                if held { "fail" } else { "pass" },
                held.then_some("/var/lib/rustic/last-snapshot is 26h old (max_age 25h)"),
                read,
                None,
            ),
            check(
                "drift", "common", "metrics", "tcp", "pass", None, read, None,
            ),
        ]
    };
    match host.id.as_str() {
        "pi-5" => health_results.extend([
            check(
                "baseline",
                "nomad",
                "api",
                "http",
                "pass",
                None,
                13,
                Some("42"),
            ),
            check(
                "bake",
                "nomad",
                "api",
                "http",
                "flap",
                Some("answered 500 for 20s while nomad restarted, then 200"),
                10,
                Some("42"),
            ),
            check(
                "bake",
                "nomad",
                "api",
                "http",
                "fail",
                Some("answered 500 for longer than settle (2m)"),
                6,
                Some("42"),
            ),
            check(
                "bake",
                "nomad",
                "unit",
                "systemd",
                "pass",
                None,
                6,
                Some("42"),
            ),
        ]),
        "pi-3" | "pi-1" | "pi-2" => health_results.extend([
            check(
                "baseline",
                "nomad",
                "api",
                "http",
                "pass",
                None,
                6,
                Some("42"),
            ),
            check("bake", "nomad", "api", "http", "pass", None, 1, Some("42")),
            check(
                "bake",
                "nomad",
                "unit",
                "systemd",
                "pass",
                Some("active for 4m (min_uptime 30s)"),
                1,
                Some("42"),
            ),
        ]),
        "nas-1" => health_results.push(check(
            "baseline",
            "backup",
            "marker",
            "file",
            "fail",
            Some("/var/lib/rustic/last-snapshot is 26h old (max_age 25h)"),
            3,
            Some("42"),
        )),
        _ => {}
    }
    let recent_runs = super::runs::summaries(host, now);
    let latest_run = recent_runs
        .first()
        .map(|r| super::runs::report_for(&host.id, r, now));
    let drift = if host.drifted {
        vec![
            DriftItem {
                module: "common".into(),
                kind: "sysctl.param".into(),
                key: "vm.swappiness".into(),
                summary: "1 → 60 (changed outside Buckle)".into(),
                unified: Some("--- applied\n+++ current\n@@ -1 +1 @@\n-vm.swappiness = 1\n+vm.swappiness = 60\n".into()),
                detected_at: Some(now - Duration::minutes(12)),
            },
            DriftItem {
                module: "nomad".into(),
                kind: "file.template".into(),
                key: "/etc/nomad.d/nomad.hcl".into(),
                summary: "2 lines differ from the rendered template".into(),
                unified: Some("--- applied\n+++ current\n@@ -6,4 +6,4 @@\n client {\n   enabled = true\n-  servers = [\"«secret:nomad_server@3f9a»\"]\n+  servers = [\"«secret:nomad_server@0000»\"]\n-  node_class = \"worker\"\n+  node_class = \"debug\"\n }\n".into()),
                detected_at: Some(now - Duration::minutes(12)),
            },
        ]
    } else {
        vec![]
    };
    HostDetail {
        summary: host.clone(),
        facts: if pending {
            json!({"os": {"family": "debian"}, "net": {"hostname": host.id}})
        } else {
            facts(host)
        },
        facts_at: Some(now - Duration::minutes(4)),
        resources,
        recent_runs,
        health_checks: host_checks(&health_results, &host.id),
        health_results,
        drift,
        latest_run,
    }
}

/// Each check's status per rollout, as the server aggregates it: the demo's
/// readings make up rollout 42 (and the drift readings since), and two
/// earlier rollouts passed, except on pi-5 whose api blipped in rollout 41.
fn host_checks(results: &[HealthResultDto], host: &str) -> Vec<HostCheck> {
    let mut checks: BTreeMap<(String, String), HostCheck> = BTreeMap::new();
    let mut sorted: Vec<&HealthResultDto> = results.iter().collect();
    sorted.sort_by_key(|r| r.at);
    let rank = |s: &str| match s {
        "fail" => 2,
        "pass" => 0,
        _ => 1,
    };
    for r in sorted {
        let c = checks
            .entry((r.module.clone(), r.check.clone()))
            .or_insert_with(|| {
                let earlier = |id: &str, days: i64, worst: &str| CheckEpoch {
                    rollout_id: Some(id.into()),
                    revision: format!("{id}e1d2c3b4a5f6"),
                    status: "pass".into(),
                    worst: worst.into(),
                    reason: None,
                    readings: 40,
                    first_at: r.at - Duration::days(days),
                    last_at: r.at - Duration::days(days - 1),
                };
                let blip = if host == "pi-5" && r.check == "api" {
                    "fail"
                } else {
                    "pass"
                };
                HostCheck {
                    module: r.module.clone(),
                    check: r.check.clone(),
                    kind: r.kind.clone(),
                    required: r.required,
                    status: r.status.clone(),
                    reason: r.reason.clone(),
                    scope: r.scope.clone(),
                    at: r.at,
                    history: vec![earlier("41", 3, blip), earlier("40", 6, "pass")],
                }
            });
        c.status = r.status.clone();
        c.reason = r.reason.clone();
        c.scope = r.scope.clone();
        c.at = r.at;
        if c.history[0].rollout_id.as_deref() != Some("42") {
            c.history.insert(
                0,
                CheckEpoch {
                    rollout_id: Some("42".into()),
                    revision: "42a1b2c3d4e5".into(),
                    status: r.status.clone(),
                    worst: r.status.clone(),
                    reason: r.reason.clone(),
                    readings: 0,
                    first_at: r.at,
                    last_at: r.at,
                },
            );
        }
        let e = &mut c.history[0];
        e.status = r.status.clone();
        e.reason = r.reason.clone();
        e.readings += 1;
        e.last_at = r.at;
        if rank(&r.status) > rank(&e.worst) {
            e.worst = r.status.clone();
        }
    }
    checks.into_values().collect()
}

fn kind(
    kind: &str,
    description: &str,
    platforms: &[&str],
    locks: &[&str],
    removal: Option<&str>,
    caution: bool,
    schema: Value,
) -> KindDescriptor {
    KindDescriptor {
        kind: kind.into(),
        description: description.into(),
        platforms: platforms.iter().map(|s| s.to_string()).collect(),
        locks: locks.iter().map(|s| s.to_string()).collect(),
        removal_default: removal.map(Into::into),
        removal_caution: caution,
        schema,
    }
}

fn perms() -> Value {
    json!({"type": "object", "properties": {
        "mode": {"type": ["string", "null"], "description": "Octal file mode, e.g. `0644`."},
        "owner": {"type": ["string", "null"], "description": "Owning user."},
        "group": {"type": ["string", "null"], "description": "Owning group."}
    }})
}

pub fn catalog() -> Catalog {
    let linux = &["linux"];
    let debian = &["debian", "ubuntu"];
    Catalog {
        server_version: SERVER_VERSION.into(),
        tasks: vec![
            kind(
                "apt.package",
                "Install, upgrade or remove Debian packages.",
                debian,
                &["apt"],
                Some("cleanup"),
                true,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}], "description": "A package or a list of packages."},
                        "version": {"type": ["string", "null"], "description": "Pin to this version (`1.9.3-1`)."},
                        "state": {"$ref": "#/$defs/PackageState", "default": "present"},
                        "update_cache": {"type": "boolean", "default": false, "description": "Run `apt-get update` first when the cache is older than an hour."}
                    },
                    "required": ["name"],
                    "$defs": {"PackageState": {"oneOf": [
                        {"const": "present", "description": "Installed at any version."},
                        {"const": "latest", "description": "Installed and upgraded to the newest candidate."},
                        {"const": "absent", "description": "Removed (destructive)."}
                    ]}}
                }),
            ),
            kind(
                "file.template",
                "Render a minijinja template from the module's files/ to a path on the host.",
                linux,
                &[],
                Some("cleanup"),
                false,
                json!({
                    "type": "object",
                    "properties": {
                        "src": {"type": "string", "description": "Template path, relative to the module's `files/` directory."},
                        "dest": {"type": "string", "description": "Absolute destination path on the host."},
                        "validate": {"type": ["array", "null"], "items": {"type": "string"}, "default": null, "description": "A command that checks the rendered file; `{path}` stands for the file to check."},
                        "backup": {"type": "boolean", "default": false, "description": "Keep a timestamped `.bak` copy of the destination before replacing its content."},
                        "vars": {"type": "object", "additionalProperties": {"$ref": "#/$defs/SecretField"}, "description": "Extra template variables; values may reference secrets."}
                    },
                    "allOf": [{"$ref": "#/$defs/FilePerms"}],
                    "required": ["src", "dest"],
                    "$defs": {
                        "FilePerms": perms(),
                        "SecretField": {"type": "string", "description": "A literal or a template; may reference secrets (`{{ secrets.NAME }}`).", "x-buckle-secret": true}
                    }
                }),
            ),
            kind(
                "file.absent",
                "Make sure a path does not exist.",
                linux,
                &[],
                Some("leave"),
                false,
                json!({
                    "type": "object",
                    "properties": {"path": {"type": "string", "description": "Absolute path to remove."}, "recursive": {"type": "boolean", "default": false}},
                    "required": ["path"]
                }),
            ),
            kind(
                "systemd.unit",
                "Enable, start or stop a systemd unit, optionally writing its unit file.",
                linux,
                &["systemd"],
                Some("cleanup"),
                false,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Unit name, e.g. `nomad.service`."},
                        "enabled": {"type": ["boolean", "null"], "default": true},
                        "state": {"oneOf": [{"const": "started"}, {"const": "stopped"}, {"const": "restarted"}], "default": "started"},
                        "content": {"type": ["string", "null"], "description": "The unit file's content, when Buckle owns it."}
                    },
                    "required": ["name"]
                }),
            ),
            kind(
                "user.account",
                "Manage a local user account.",
                linux,
                &["passwd"],
                Some("cleanup"),
                true,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "uid": {"type": ["integer", "null"], "format": "uint32"},
                        "groups": {"type": "array", "items": {"type": "string"}, "default": []},
                        "shell": {"type": "string", "default": "/bin/bash"},
                        "password_hash": {"anyOf": [{"$ref": "#/$defs/SecretField"}, {"type": "null"}], "description": "A crypt(3) hash; reference a secret rather than writing it here."},
                        "state": {"oneOf": [{"const": "present"}, {"const": "absent"}], "default": "present"}
                    },
                    "required": ["name"],
                    "$defs": {"SecretField": {"type": "string", "x-buckle-secret": true}}
                }),
            ),
            kind(
                "command.run",
                "Run a command, guarded by `creates`/`unless` for idempotency.",
                linux,
                &[],
                Some("leave"),
                false,
                json!({
                    "type": "object",
                    "properties": {
                        "run": {"type": "array", "items": {"type": "string"}, "description": "argv; no shell."},
                        "creates": {"type": ["string", "null"], "description": "Skip when this path exists."},
                        "unless": {"type": ["array", "null"], "items": {"type": "string"}, "description": "Skip when this command exits 0."},
                        "env": {"type": "object", "additionalProperties": {"$ref": "#/$defs/SecretField"}, "default": {}}
                    },
                    "required": ["run"],
                    "$defs": {"SecretField": {"type": "string", "x-buckle-secret": true}}
                }),
            ),
            kind(
                "sysctl.param",
                "Set a kernel parameter now and on boot.",
                linux,
                &[],
                Some("leave"),
                false,
                json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}, "value": {"type": "string"}, "persist": {"type": "boolean", "default": true}},
                    "required": ["name", "value"]
                }),
            ),
        ],
        facts: vec![
            kind(
                "os",
                "Distribution, version, kernel and architecture.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
            kind(
                "hardware",
                "CPU count, memory and board model.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
            kind(
                "net",
                "Hostname, FQDN and interfaces with their addresses.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
            kind(
                "tailscale",
                "Tailscale version, state, tags and address.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
            kind(
                "docker",
                "Docker version and running container count.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
            kind(
                "nomad",
                "Nomad version, node class and eligibility.",
                linux,
                &[],
                None,
                false,
                Value::Null,
            ),
        ],
        health: vec![
            kind(
                "http",
                "An HTTP request answers with the expected status (and body).",
                linux,
                &[],
                None,
                false,
                json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "format": "uri"},
                        "method": {"type": "string", "default": "GET"},
                        "headers": {"type": "object", "additionalProperties": {"$ref": "#/$defs/SecretField"}, "default": {}},
                        "expect": {"$ref": "#/$defs/Expect"},
                        "tls": {"anyOf": [{"$ref": "#/$defs/Tls"}, {"type": "null"}]}
                    },
                    "required": ["url"],
                    "$defs": {
                        "SecretField": {"type": "string", "x-buckle-secret": true},
                        "Expect": {"type": "object", "properties": {"status": {"type": "integer", "default": 200}, "body_contains": {"type": ["string", "null"]}}},
                        "Tls": {"type": "object", "properties": {"insecure": {"type": "boolean", "default": false}}}
                    }
                }),
            ),
            kind(
                "tcp",
                "A TCP port accepts connections.",
                linux,
                &[],
                None,
                false,
                json!({"type": "object", "properties": {"address": {"type": "string", "description": "`host:port`."}}, "required": ["address"]}),
            ),
            kind(
                "systemd",
                "A unit is in the expected state and has stayed up.",
                linux,
                &[],
                None,
                false,
                json!({"type": "object", "properties": {"unit": {"type": "string"}, "state": {"oneOf": [{"const": "active"}, {"const": "inactive"}], "default": "active"}, "min_uptime": {"type": ["string", "null"], "description": "e.g. `30s`: catches crash loops inside the bake window."}}, "required": ["unit"]}),
            ),
            kind(
                "command",
                "A command exits as expected.",
                linux,
                &[],
                None,
                false,
                json!({"type": "object", "properties": {"run": {"type": "array", "items": {"type": "string"}}, "expect": {"type": "object", "properties": {"exit": {"type": "integer", "default": 0}, "stdout_matches": {"type": ["string", "null"]}}}}, "required": ["run"]}),
            ),
            kind(
                "filter",
                "A filt-rs expression over refreshed facts.",
                &["any"],
                &[],
                None,
                false,
                json!({"type": "object", "properties": {"expression": {"type": "string", "description": "e.g. `host.facts.disk.root.free_percent > 10`."}}, "required": ["expression"]}),
            ),
            kind(
                "file",
                "A path exists and was modified recently.",
                linux,
                &[],
                None,
                false,
                json!({"type": "object", "properties": {"path": {"type": "string"}, "exists": {"type": "boolean", "default": true}, "max_age": {"type": ["string", "null"]}}, "required": ["path"]}),
            ),
        ],
    }
}
