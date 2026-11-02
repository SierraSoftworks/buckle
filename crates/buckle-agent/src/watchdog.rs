//! The local rollback watchdog (design §10, "Agents can roll themselves back
//! when a change cuts them off from the server").
//!
//! After an apply, if the agent was connected before it, the watchdog watches
//! for `local_rollback.after`: whether a session comes back, and (with
//! `on_health`) whether required checks keep failing past `settle`. The agent
//! rolls itself back to the cached stable revision only when it was connected
//! before the apply, has had no session for the whole window, and (with
//! `on_health`) health is failing too. While the server is reachable the
//! agent never decides alone.
//!
//! This module is the pure decision logic; the session loop feeds it health
//! reports and connectivity, and persists [`WatchdogState`] so a restart
//! mid-window resumes the window.

use buckle_core::health::{HealthReport, HealthResult};
use buckle_core::model::LocalRollbackPolicy;
use buckle_core::plan::RevisionId;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::Instant;

/// The persisted part of an armed watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogState {
    /// The revision just applied.
    pub from: RevisionId,
    /// The cached stable revision to fall back to.
    pub to: RevisionId,
    /// When the window closes (wall clock, so it survives restarts).
    pub deadline_at: chrono::DateTime<chrono::Utc>,
    /// The window length, for messages.
    #[serde(with = "humantime_serde")]
    pub after: Duration,
    /// Whether failing health is required as well as lost connectivity.
    pub on_health: bool,
    /// How long a required check must fail continuously to count.
    #[serde(with = "humantime_serde")]
    pub settle: Duration,
    /// Checks waived for the apply (`module/check`); they never count.
    #[serde(default)]
    pub waived_checks: Vec<String>,
}

/// What the watchdog wants done now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Keep watching.
    Wait,
    /// The window passed safely (or a session came back): stop watching.
    Disarm,
    /// Apply the stable revision now.
    RollBack {
        /// Why, for the LocalRollback report.
        reason: String,
    },
}

/// An armed watchdog with its in-process clocks.
#[derive(Debug, Clone)]
pub struct Watchdog {
    /// The persisted state.
    pub state: WatchdogState,
    deadline: Instant,
    failing_since: Option<Instant>,
    evidence: Vec<HealthResult>,
}

impl Watchdog {
    /// Arms a watchdog after applying `from`, falling back to `to`.
    pub fn arm(
        from: RevisionId,
        to: RevisionId,
        policy: &LocalRollbackPolicy,
        settle: Duration,
        waived_checks: Vec<String>,
    ) -> Self {
        let deadline_at = chrono::Utc::now()
            + chrono::Duration::from_std(policy.after).unwrap_or(chrono::Duration::MAX);
        Self {
            state: WatchdogState {
                from,
                to,
                deadline_at,
                after: policy.after,
                on_health: policy.on_health,
                settle,
                waived_checks,
            },
            deadline: Instant::now() + policy.after,
            failing_since: None,
            evidence: Vec::new(),
        }
    }

    /// Resumes a persisted watchdog, e.g. after a restart mid-window. The
    /// remaining window is taken from the wall clock; failing health has to be
    /// observed for `settle` again.
    pub fn resume(state: WatchdogState) -> Self {
        let remaining = (state.deadline_at - chrono::Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        Self {
            state,
            deadline: Instant::now() + remaining,
            failing_since: None,
            evidence: Vec::new(),
        }
    }

    /// How often health should be sampled while disconnected.
    pub fn check_interval(&self) -> Duration {
        (self.state.settle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30))
    }

    /// The failing checks seen most recently.
    pub fn evidence(&self) -> &[HealthResult] {
        &self.evidence
    }

    /// Records a health sample taken at `now`.
    pub fn observe(&mut self, report: &HealthReport, now: Instant) {
        let failures: Vec<HealthResult> = report
            .failures(&self.state.waived_checks)
            .cloned()
            .collect();
        if failures.is_empty() {
            self.failing_since = None;
            self.evidence.clear();
        } else {
            self.failing_since.get_or_insert(now);
            self.evidence = failures;
        }
    }

    /// Decides what to do at `now`, given whether a session is up.
    pub fn decide(&self, connected: bool, now: Instant) -> Verdict {
        if now < self.deadline {
            return Verdict::Wait;
        }
        if connected {
            return Verdict::Disarm;
        }
        let window = humantime_serde::re::humantime::format_duration(self.state.after);
        if !self.state.on_health {
            return Verdict::RollBack {
                reason: format!(
                    "no session with the server for {window} after applying {}",
                    self.state.from
                ),
            };
        }
        match self.failing_since {
            None => Verdict::Disarm,
            Some(since) if now.duration_since(since) >= self.state.settle => {
                let checks: Vec<String> = self
                    .evidence
                    .iter()
                    .map(|r| format!("{}/{}", r.module, r.check))
                    .collect();
                Verdict::RollBack {
                    reason: format!(
                        "no session with the server for {window} after applying {} and required checks failing for longer than {}: {}",
                        self.state.from,
                        humantime_serde::re::humantime::format_duration(self.state.settle),
                        checks.join(", ")
                    ),
                }
            }
            Some(_) => Verdict::Wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::health::{HealthScope, HealthStatus};
    use rstest::rstest;

    fn policy(on_health: bool) -> LocalRollbackPolicy {
        LocalRollbackPolicy {
            enabled: true,
            after: Duration::from_secs(300),
            on_health,
        }
    }

    fn report(failing: bool) -> HealthReport {
        HealthReport {
            scope: HealthScope::Bake,
            results: vec![HealthResult {
                module: "nomad".into(),
                check: "api".into(),
                kind: "http".into(),
                required: true,
                status: if failing {
                    HealthStatus::fail("connection refused")
                } else {
                    HealthStatus::Pass
                },
                at: chrono::Utc::now(),
                duration_ms: 1,
            }],
        }
    }

    fn armed(on_health: bool) -> Watchdog {
        Watchdog::arm(
            RevisionId::new("new"),
            RevisionId::new("stable"),
            &policy(on_health),
            Duration::from_secs(120),
            vec![],
        )
    }

    #[tokio::test(start_paused = true)]
    async fn waits_until_the_window_closes() {
        let w = armed(false);
        assert_eq!(w.decide(false, Instant::now()), Verdict::Wait);
        assert!(matches!(
            w.decide(false, Instant::now() + Duration::from_secs(300)),
            Verdict::RollBack { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_at_the_deadline_disarms() {
        let w = armed(false);
        assert_eq!(
            w.decide(true, Instant::now() + Duration::from_secs(301)),
            Verdict::Disarm
        );
    }

    #[rstest]
    #[case::healthy(false, None, Verdict::Disarm)]
    #[case::failing_long_enough(true, Some(150), Verdict::RollBack { reason: String::new() })]
    #[case::failing_too_briefly(true, Some(60), Verdict::Wait)]
    #[tokio::test(start_paused = true)]
    async fn on_health_requires_failures_past_settle(
        #[case] failing: bool,
        #[case] failing_for: Option<u64>,
        #[case] expected: Verdict,
    ) {
        let mut w = armed(true);
        let deadline = Instant::now() + Duration::from_secs(300);
        let observed_at = deadline - Duration::from_secs(failing_for.unwrap_or(10));
        w.observe(&report(failing), observed_at);
        let verdict = w.decide(false, deadline);
        match (verdict, expected) {
            (Verdict::RollBack { reason }, Verdict::RollBack { .. }) => {
                assert!(reason.contains("nomad/api"), "{reason}")
            }
            (got, want) => assert_eq!(got, want),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_resets_the_settle_clock_and_waivers_never_count() {
        let mut w = armed(true);
        let deadline = Instant::now() + Duration::from_secs(300);
        w.observe(&report(true), Instant::now());
        w.observe(&report(false), Instant::now() + Duration::from_secs(10));
        assert_eq!(w.decide(false, deadline), Verdict::Disarm);

        let mut waived = Watchdog::arm(
            RevisionId::new("new"),
            RevisionId::new("stable"),
            &policy(true),
            Duration::from_secs(120),
            vec!["nomad/api".into()],
        );
        waived.observe(&report(true), Instant::now());
        assert_eq!(waived.decide(false, deadline), Verdict::Disarm);
    }

    #[tokio::test(start_paused = true)]
    async fn resuming_keeps_the_wall_clock_deadline() {
        let w = armed(false);
        let mut state = w.state.clone();
        state.deadline_at = chrono::Utc::now() + chrono::Duration::seconds(60);
        let resumed = Watchdog::resume(state);
        assert_eq!(resumed.decide(false, Instant::now()), Verdict::Wait);
        assert!(matches!(
            resumed.decide(false, Instant::now() + Duration::from_secs(61)),
            Verdict::RollBack { .. }
        ));
    }
}
