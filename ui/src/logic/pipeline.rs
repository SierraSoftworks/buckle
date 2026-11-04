//! Lays a rollout out as the pipeline the rollout page draws (design §12):
//! stages as columns in phase order, every host as one block in exactly one
//! stage, blocks wrapping onto as many rows as a large stage needs, and a gate
//! before each stage saying whether the rollout may pass it.

use std::collections::{BTreeMap, BTreeSet};

use buckle_api::{
    HostRolloutState, HostSummary, PhaseState, PlanGroup, RolloutDetail, RolloutHost, RolloutPhase,
};

/// The fewest blocks a row holds: a stage column is never narrower than its
/// header needs, however few hosts it has.
pub const MIN_PER_ROW: usize = 3;
/// The most blocks a row holds before a stage grows downwards instead.
pub const MAX_PER_ROW: usize = 10;

/// How the gate in front of a stage is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// The rollout passes once the previous stage has baked and re-checked.
    Auto,
    /// An operator must approve the stage's plans (`approval: required`, or
    /// `if_destructive` with a destructive plan in the stage).
    Approval,
}

/// Where the rollout stands at a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    /// Not reached yet.
    Waiting,
    /// Approved ahead of being reached.
    Approved,
    /// Reached, and waiting for an approval.
    AwaitingApproval,
    /// Passed.
    Open,
    /// The stage behind it is paused.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub kind: GateKind,
    pub state: GateState,
    pub approved_by: Option<String>,
    /// Whether the approval is required only because a plan in the stage is
    /// destructive (`approval: if_destructive`).
    pub because_destructive: bool,
}

impl Gate {
    pub fn label(&self) -> &'static str {
        match (self.kind, self.state) {
            (_, GateState::Blocked) => "paused",
            (GateKind::Approval, GateState::AwaitingApproval) => "needs approval",
            (GateKind::Approval, GateState::Approved) => "approved",
            (GateKind::Approval, GateState::Open) => "approved",
            (GateKind::Approval, GateState::Waiting) => "approval required",
            (GateKind::Auto, GateState::Open) => "passed",
            (GateKind::Auto, _) => "health gate",
        }
    }

    /// Whether the gate is shut in front of the rollout right now.
    pub fn closed(&self) -> bool {
        matches!(self.state, GateState::AwaitingApproval | GateState::Blocked)
    }
}

/// What a stage's header summarises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageCounts {
    pub total: usize,
    pub healthy: usize,
    pub failed: usize,
    pub active: usize,
    pub held: usize,
    pub plan_pending: usize,
    pub needs_approval: usize,
    pub skipped: usize,
    pub rolled_back: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stage<'a> {
    pub phase: &'a RolloutPhase,
    /// The blocks, row by row.
    pub rows: Vec<Vec<&'a RolloutHost>>,
    pub per_row: usize,
    pub counts: StageCounts,
    pub gate: Gate,
}

impl Stage<'_> {
    pub fn hosts(&self) -> impl Iterator<Item = &RolloutHost> {
        self.rows.iter().flatten().copied()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline<'a> {
    pub stages: Vec<Stage<'a>>,
    /// Hosts the server listed in more than one stage. Each is drawn once,
    /// in the stage its newest entry names; this is for a diagnostic note.
    pub duplicates: Vec<String>,
}

/// How many blocks a row of an `n`-host stage holds: about one and a half
/// times as wide as it is tall, so a large stage stays readable instead of
/// becoming one very long column or one very wide row.
pub fn per_row(n: usize) -> usize {
    let ideal = ((n as f64) * 1.5).sqrt().ceil() as usize;
    ideal.clamp(MIN_PER_ROW, MAX_PER_ROW)
}

/// Assigns every host to exactly one stage and lays the stages out.
///
/// A host the server lists more than once (it moved to the stragglers stage,
/// or a live update named a new phase) belongs to the stage named by its most
/// recently updated entry, falling back to the stage it was listed under when
/// that name matches no stage.
pub fn layout<'a>(detail: &'a RolloutDetail) -> Pipeline<'a> {
    let names: BTreeSet<&str> = detail.phases.iter().map(|p| p.name.as_str()).collect();

    // host → (stage index, entry)
    let mut chosen: BTreeMap<&str, (usize, &RolloutHost)> = BTreeMap::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, phase) in detail.phases.iter().enumerate() {
        for entry in &phase.hosts {
            *seen.entry(entry.host.as_str()).or_default() += 1;
            let stage = if names.contains(entry.phase.as_str()) {
                detail
                    .phases
                    .iter()
                    .position(|p| p.name == entry.phase)
                    .unwrap_or(index)
            } else {
                index
            };
            match chosen.get(entry.host.as_str()) {
                Some((_, current)) if current.updated_at >= entry.updated_at => {}
                _ => {
                    chosen.insert(entry.host.as_str(), (stage, entry));
                }
            }
        }
    }

    let mut members: Vec<Vec<&RolloutHost>> = vec![Vec::new(); detail.phases.len()];
    for (stage, entry) in chosen.into_values() {
        members[stage].push(entry);
    }

    let stages = detail
        .phases
        .iter()
        .zip(members)
        .map(|(phase, mut hosts)| {
            hosts.sort_by(|a, b| {
                (a.wave.unwrap_or(u32::MAX), &a.host).cmp(&(b.wave.unwrap_or(u32::MAX), &b.host))
            });
            let per_row = per_row(hosts.len());
            let counts = counts(&hosts);
            let gate = gate(phase, &hosts, &detail.plans);
            let rows = hosts.chunks(per_row).map(<[_]>::to_vec).collect();
            Stage {
                phase,
                rows,
                per_row,
                counts,
                gate,
            }
        })
        .collect();

    let duplicates = seen
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(host, _)| host.to_string())
        .collect();

    Pipeline { stages, duplicates }
}

fn counts(hosts: &[&RolloutHost]) -> StageCounts {
    let mut c = StageCounts {
        total: hosts.len(),
        ..StageCounts::default()
    };
    for h in hosts {
        match h.state {
            HostRolloutState::Healthy => c.healthy += 1,
            HostRolloutState::Failed => c.failed += 1,
            HostRolloutState::Applying | HostRolloutState::Rebooting | HostRolloutState::Baking => {
                c.active += 1
            }
            HostRolloutState::Held => c.held += 1,
            HostRolloutState::PlanPending => c.plan_pending += 1,
            HostRolloutState::Skipped => c.skipped += 1,
            HostRolloutState::RolledBack => c.rolled_back += 1,
            HostRolloutState::Pending | HostRolloutState::Planned => {}
        }
        if h.needs_approval {
            c.needs_approval += 1;
        }
    }
    c
}

/// Whether any distinct plan used by these hosts is destructive.
pub fn destructive_in(hosts: &[&RolloutHost], plans: &[PlanGroup]) -> bool {
    hosts.iter().any(|h| {
        plans.iter().any(|g| {
            g.destructive
                && (h.plan_digest.as_deref() == Some(g.digest.as_str())
                    || g.hosts.contains(&h.host))
        })
    })
}

/// Whether any distinct plan used by these hosts has a deferred module, whose
/// plan is only known at apply time: `if_destructive` reviews it as if it
/// were destructive, as the server does.
fn deferred_in(hosts: &[&RolloutHost], plans: &[PlanGroup]) -> bool {
    hosts.iter().any(|h| {
        plans.iter().any(|g| {
            g.plan.as_ref().is_some_and(|p| !p.deferred.is_empty())
                && (h.plan_digest.as_deref() == Some(g.digest.as_str())
                    || g.hosts.contains(&h.host))
        })
    })
}

fn gate(phase: &RolloutPhase, hosts: &[&RolloutHost], plans: &[PlanGroup]) -> Gate {
    let policy = phase.approval.to_ascii_lowercase();
    // The server is the authority on whether the stage waits for an
    // approval: a phase it holds, or a host it says needs one, is gated
    // whatever the plans here suggest.
    let server_asks =
        phase.state == PhaseState::AwaitingApproval || hosts.iter().any(|h| h.needs_approval);
    let because_destructive = policy == "if_destructive"
        && (server_asks || destructive_in(hosts, plans) || deferred_in(hosts, plans));
    let kind = if policy == "required" || because_destructive || server_asks {
        GateKind::Approval
    } else {
        GateKind::Auto
    };
    let state = match phase.state {
        PhaseState::Paused => GateState::Blocked,
        PhaseState::AwaitingApproval => GateState::AwaitingApproval,
        PhaseState::Running | PhaseState::Baking | PhaseState::Done | PhaseState::Open => {
            GateState::Open
        }
        PhaseState::Skipped => GateState::Open,
        PhaseState::Pending if kind == GateKind::Approval && phase.approved_by.is_some() => {
            GateState::Approved
        }
        PhaseState::Pending => GateState::Waiting,
    };
    Gate {
        kind,
        state,
        approved_by: phase.approved_by.clone(),
        because_destructive,
    }
}

/// The stage the fleet map puts hosts the rollout did not include in.
pub const OUTSIDE_STAGE: &str = "not in this rollout";

/// The fleet laid over a rollout's stages, for the Overview's map: the
/// rollout's hosts that are still in the fleet where the rollout put them,
/// and every other host in a trailing [`OUTSIDE_STAGE`].
pub fn fleet_map(
    detail: &RolloutDetail,
    fleet: &[HostSummary],
    clusters: &[HostSummary],
) -> RolloutDetail {
    let fleet: Vec<&HostSummary> = fleet.iter().chain(clusters).collect();
    let known: BTreeSet<&str> = fleet.iter().map(|h| h.id.as_str()).collect();
    let mut map = detail.clone();
    for phase in &mut map.phases {
        phase.hosts.retain(|h| known.contains(h.host.as_str()));
    }
    let placed: BTreeSet<&str> = detail
        .phases
        .iter()
        .flat_map(|p| &p.hosts)
        .map(|h| h.host.as_str())
        .collect();
    let outside: Vec<RolloutHost> = fleet
        .iter()
        .filter(|h| !placed.contains(h.id.as_str()))
        .map(|h| RolloutHost {
            host: h.id.clone(),
            phase: OUTSIDE_STAGE.into(),
            wave: None,
            state: HostRolloutState::Pending,
            plan_digest: None,
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: h.last_seen.unwrap_or(h.first_seen),
            bake_until: None,
            reboot: None,
            kind: h.kind,
            stage: 0,
            executor: None,
            bake: None,
            preview: false,
            drifted_from: None,
            needs_approval: false,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        })
        .collect();
    if !outside.is_empty() {
        map.phases.push(RolloutPhase {
            name: OUTSIDE_STAGE.into(),
            state: PhaseState::Pending,
            parallel: String::new(),
            approval: "auto".into(),
            bake_seconds: 0,
            max_failures: String::new(),
            virtual_stage: true,
            hosts: outside,
            approved_by: None,
            approved_at: None,
            bake_until: None,
            health_checks: vec![],
            stages: 1,
        });
    }
    map
}

/// Where one check of a phase's health gate stands on one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStanding {
    Failing,
    Waived,
    Passing,
    /// The host has not been held to it yet (or was skipped).
    Pending,
}

impl CheckStanding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Failing => "failing",
            Self::Waived => "waived",
            Self::Passing => "passing",
            Self::Pending => "not checked",
        }
    }

    pub fn tone(self) -> super::tone::Tone {
        use super::tone::Tone;
        match self {
            Self::Failing => Tone::Danger,
            Self::Waived => Tone::Mute,
            Self::Passing => Tone::Ok,
            Self::Pending => Tone::Neutral,
        }
    }
}

/// How each check of `phase`'s health gate stands on `host`, in the gate's
/// order, followed by any failing check the gate does not list.
pub fn host_checks(phase: &RolloutPhase, host: &RolloutHost) -> Vec<(String, CheckStanding)> {
    let checked = matches!(
        host.state,
        HostRolloutState::Healthy | HostRolloutState::Baking
    );
    let extra = host
        .failed_checks
        .iter()
        .filter(|c| !phase.health_checks.contains(c));
    phase
        .health_checks
        .iter()
        .chain(extra)
        .map(|c| {
            let standing = if host.failed_checks.contains(c) {
                CheckStanding::Failing
            } else if host.waived_checks.contains(c) {
                CheckStanding::Waived
            } else if checked {
                CheckStanding::Passing
            } else {
                CheckStanding::Pending
            };
            (c.clone(), standing)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{RolloutState, RolloutSummary};
    use chrono::{Duration, Utc};

    fn host(name: &str, phase: &str, state: HostRolloutState, age_secs: i64) -> RolloutHost {
        RolloutHost {
            host: name.into(),
            phase: phase.into(),
            wave: Some(1),
            state,
            plan_digest: Some("aaaa".into()),
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now() - Duration::seconds(age_secs),
            bake_until: None,
            reboot: None,
            kind: Default::default(),
            stage: 0,
            executor: None,
            bake: None,
            preview: false,
            drifted_from: None,
            needs_approval: false,
            needs_attention: false,
            error_detail: None,
            run_id: None,
        }
    }

    fn phase(
        name: &str,
        state: PhaseState,
        approval: &str,
        hosts: Vec<RolloutHost>,
    ) -> RolloutPhase {
        RolloutPhase {
            name: name.into(),
            state,
            parallel: "1".into(),
            approval: approval.into(),
            bake_seconds: 300,
            max_failures: "0".into(),
            virtual_stage: name == "stragglers",
            hosts,
            approved_by: None,
            approved_at: None,
            bake_until: None,
            health_checks: vec![],
            stages: 1,
        }
    }

    fn detail(phases: Vec<RolloutPhase>, plans: Vec<PlanGroup>) -> RolloutDetail {
        RolloutDetail {
            summary: RolloutSummary {
                id: "42".into(),
                name: None,
                revision: "7c2e1f0".into(),
                revision_message: "m".into(),
                state: RolloutState::Rolling,
                started_at: Utc::now(),
                finished_at: None,
                triggered_by: "controller".into(),
                hosts_total: 0,
                hosts_done: 0,
                hosts_failed: 0,
                current_phase: None,
                pause_reason: None,
                end_reason: None,
                superseded_by: None,
                trace_id: None,
                trace_url: None,
            },
            phases,
            plans,
            fleet: None,
            events: vec![],
        }
    }

    fn names(stage: &Stage) -> Vec<String> {
        stage.hosts().map(|h| h.host.clone()).collect()
    }

    #[test]
    fn every_host_is_drawn_once_in_the_stage_its_newest_entry_names() {
        let d = detail(
            vec![
                phase(
                    "canary",
                    PhaseState::Done,
                    "required",
                    vec![host("pi-1", "canary", HostRolloutState::Healthy, 60)],
                ),
                phase(
                    "cloud",
                    PhaseState::Running,
                    "auto",
                    vec![
                        // Missed its wave: the newer entry moves it to stragglers.
                        host("az-1", "cloud", HostRolloutState::PlanPending, 120),
                        host("oci-1", "cloud", HostRolloutState::Applying, 10),
                        // Listed again by a later stage's selector: still once.
                        host("pi-1", "cloud", HostRolloutState::Pending, 600),
                    ],
                ),
                phase(
                    "stragglers",
                    PhaseState::Open,
                    "auto",
                    vec![host("az-1", "stragglers", HostRolloutState::Pending, 5)],
                ),
            ],
            vec![],
        );
        let p = layout(&d);
        assert_eq!(names(&p.stages[0]), vec!["pi-1"]);
        assert_eq!(names(&p.stages[1]), vec!["oci-1"]);
        assert_eq!(names(&p.stages[2]), vec!["az-1"]);
        let total: usize = p.stages.iter().map(|s| s.counts.total).sum();
        assert_eq!(total, 3, "three hosts, three blocks");
        assert_eq!(p.duplicates, vec!["az-1", "pi-1"]);
    }

    #[test]
    fn an_entry_naming_an_unknown_phase_stays_where_it_was_listed() {
        let d = detail(
            vec![phase(
                "home",
                PhaseState::Running,
                "auto",
                vec![host("pi-2", "renamed", HostRolloutState::Baking, 1)],
            )],
            vec![],
        );
        assert_eq!(names(&layout(&d).stages[0]), vec!["pi-2"]);
    }

    #[test]
    fn large_stages_wrap_onto_rows_that_stay_wider_than_tall() {
        assert_eq!(per_row(0), MIN_PER_ROW);
        assert_eq!(per_row(1), MIN_PER_ROW);
        assert_eq!(per_row(7), 4);
        assert_eq!(per_row(13), 5);
        assert_eq!(per_row(30), 7);
        assert_eq!(per_row(500), MAX_PER_ROW);

        let hosts = (0..13)
            .map(|i| {
                host(
                    &format!("oci-{i:02}"),
                    "cloud",
                    HostRolloutState::Planned,
                    1,
                )
            })
            .collect();
        let d = detail(
            vec![phase("cloud", PhaseState::Pending, "auto", hosts)],
            vec![],
        );
        let p = layout(&d);
        let rows: Vec<usize> = p.stages[0].rows.iter().map(Vec::len).collect();
        assert_eq!(rows, vec![5, 5, 3]);
    }

    #[test]
    fn hosts_are_ordered_by_wave_then_name() {
        let mut late = host("a-late", "home", HostRolloutState::Pending, 1);
        late.wave = Some(2);
        let mut unassigned = host("a-none", "home", HostRolloutState::Pending, 1);
        unassigned.wave = None;
        let d = detail(
            vec![phase(
                "home",
                PhaseState::Running,
                "auto",
                vec![
                    unassigned,
                    late,
                    host("z-first", "home", HostRolloutState::Baking, 1),
                ],
            )],
            vec![],
        );
        assert_eq!(
            names(&layout(&d).stages[0]),
            vec!["z-first", "a-late", "a-none"]
        );
    }

    #[test]
    fn stage_counts_summarise_the_blocks() {
        let mut stale = host("c", "home", HostRolloutState::Planned, 1);
        stale.needs_approval = true;
        let d = detail(
            vec![phase(
                "home",
                PhaseState::Running,
                "auto",
                vec![
                    host("a", "home", HostRolloutState::Healthy, 1),
                    host("b", "home", HostRolloutState::Failed, 1),
                    stale,
                    host("d", "home", HostRolloutState::Baking, 1),
                    host("e", "home", HostRolloutState::Held, 1),
                ],
            )],
            vec![],
        );
        let c = &layout(&d).stages[0].counts;
        assert_eq!(
            (
                c.total,
                c.healthy,
                c.failed,
                c.active,
                c.held,
                c.needs_approval
            ),
            (5, 1, 1, 1, 1, 1)
        );
    }

    #[test]
    fn gates_follow_the_approval_policy_and_the_phase_state() {
        let plans = vec![PlanGroup {
            digest: "aaaa".into(),
            hosts: vec!["oci-1".into()],
            changes: 3,
            removals: 1,
            destructive: true,
            plan: None,
        }];
        let mut approved_ahead = phase("late", PhaseState::Pending, "required", vec![]);
        approved_ahead.approved_by = Some("alice".into());
        let d = detail(
            vec![
                phase("canary", PhaseState::Done, "required", vec![]),
                phase(
                    "cloud",
                    PhaseState::AwaitingApproval,
                    "if_destructive",
                    vec![host("oci-1", "cloud", HostRolloutState::Planned, 1)],
                ),
                phase(
                    "rest",
                    PhaseState::Pending,
                    "if_destructive",
                    vec![{
                        let mut h = host("lab-1", "rest", HostRolloutState::Pending, 1);
                        h.plan_digest = Some("bbbb".into());
                        h
                    }],
                ),
                phase("home", PhaseState::Paused, "auto", vec![]),
                approved_ahead,
            ],
            plans,
        );
        let p = layout(&d);
        let gates: Vec<_> = p
            .stages
            .iter()
            .map(|s| (s.gate.kind, s.gate.state))
            .collect();
        assert_eq!(
            gates,
            vec![
                (GateKind::Approval, GateState::Open),
                (GateKind::Approval, GateState::AwaitingApproval),
                (GateKind::Auto, GateState::Waiting),
                (GateKind::Auto, GateState::Blocked),
                (GateKind::Approval, GateState::Approved),
            ]
        );
        assert!(p.stages[1].gate.because_destructive);
        assert!(p.stages[1].gate.closed());
        assert_eq!(p.stages[1].gate.label(), "needs approval");
        assert!(!p.stages[0].gate.closed());
    }

    /// A lone host in an `if_destructive` phase whose plan the server holds
    /// for approval, though no plan group here is marked destructive: the
    /// stage is still gated, so the page offers its approval.
    #[test]
    fn a_stage_the_server_holds_for_approval_is_gated() {
        use buckle_api::{ModuleNote, PlanView};
        let group = |deferred: bool| PlanGroup {
            digest: "aaaa".into(),
            hosts: vec!["prophet".into()],
            changes: 1,
            removals: 0,
            destructive: false,
            plan: Some(PlanView {
                deferred: if deferred {
                    vec![ModuleNote {
                        module: "otel".into(),
                        ..ModuleNote::default()
                    }]
                } else {
                    vec![]
                },
                ..PlanView::default()
            }),
        };
        let lone = |needs_approval: bool| {
            let mut h = host("prophet", "rest", HostRolloutState::Planned, 1);
            h.needs_approval = needs_approval;
            h
        };
        let cases = [
            (
                "held",
                PhaseState::AwaitingApproval,
                true,
                false,
                GateState::AwaitingApproval,
            ),
            (
                "host asks",
                PhaseState::Running,
                true,
                false,
                GateState::Open,
            ),
            (
                "deferred, not reached",
                PhaseState::Pending,
                false,
                true,
                GateState::Waiting,
            ),
        ];
        for (name, state, needs, deferred, want) in cases {
            let d = detail(
                vec![
                    phase("canary", PhaseState::Done, "auto", vec![]),
                    phase("rest", state, "if_destructive", vec![lone(needs)]),
                ],
                vec![group(deferred)],
            );
            let gate = &layout(&d).stages[1].gate;
            assert_eq!(
                (gate.kind, gate.state),
                (GateKind::Approval, want),
                "{name}"
            );
            assert!(gate.because_destructive, "{name}");
        }
    }

    #[test]
    fn a_hosts_checks_stand_by_its_own_failures_waivers_and_state() {
        use CheckStanding::*;
        let mut home = phase("home", PhaseState::Running, "auto", vec![]);
        home.health_checks = vec!["nomad/unit".into(), "caddy/http".into()];
        let standings = |h: &RolloutHost| -> Vec<CheckStanding> {
            host_checks(&home, h).into_iter().map(|(_, s)| s).collect()
        };
        let cases = [
            (
                HostRolloutState::Healthy,
                vec![],
                vec![],
                vec![Passing, Passing],
            ),
            (
                HostRolloutState::Applying,
                vec![],
                vec![],
                vec![Pending, Pending],
            ),
            (
                HostRolloutState::Failed,
                vec!["caddy/http"],
                vec![],
                vec![Pending, Failing],
            ),
            (
                HostRolloutState::Baking,
                vec![],
                vec!["nomad/unit"],
                vec![Waived, Passing],
            ),
            // A failing check outside the gate is listed after it.
            (
                HostRolloutState::Held,
                vec!["docker/daemon"],
                vec![],
                vec![Pending, Pending, Failing],
            ),
        ];
        for (state, failed, waived, want) in cases {
            let mut h = host("pi-1", "home", state, 0);
            h.failed_checks = failed.into_iter().map(Into::into).collect();
            h.waived_checks = waived.into_iter().map(Into::into).collect();
            assert_eq!(standings(&h), want, "{state:?}");
        }
    }

    #[test]
    fn fleet_map_keeps_clusters_in_their_stages() {
        let now = Utc::now();
        let mut cluster = host("home-nomad", "home", HostRolloutState::Healthy, 5);
        cluster.kind = buckle_api::TargetKind::Cluster;
        let d = detail(
            vec![phase(
                "home",
                PhaseState::Running,
                "auto",
                vec![host("pi-1", "home", HostRolloutState::Healthy, 5), cluster],
            )],
            vec![],
        );
        let summary = |id: &str, kind| HostSummary {
            id: id.into(),
            kind,
            ..crate::fixtures::data::hosts(now)[0].clone()
        };
        let hosts = [summary("pi-1", Default::default())];
        let clusters = [
            summary("home-nomad", buckle_api::TargetKind::Cluster),
            summary("lab-nomad", buckle_api::TargetKind::Cluster),
        ];
        let map = fleet_map(&d, &hosts, &clusters);
        let names = |p: &RolloutPhase| p.hosts.iter().map(|h| h.host.clone()).collect::<Vec<_>>();
        assert_eq!(names(&map.phases[0]), ["pi-1", "home-nomad"]);
        let outside = map.phases.last().unwrap();
        assert_eq!(names(outside), ["lab-nomad"]);
        assert_eq!(outside.hosts[0].kind, buckle_api::TargetKind::Cluster);
    }
}
