//! Local runs: what a caller on the agent's socket asked for, run as an
//! operation on the host's worker like any other.

// Nothing serves the socket on Windows yet, so nothing submits there.
#![cfg_attr(not(unix), allow(dead_code))]

use super::{Op, Shared, WeakQueue};
use crate::local::{Actor, Choice, Event, Request, Status};
use crate::transport::{ApplyPolicy, LocalOrigin, Report};
use buckle_core::engine::{ApplyEvent, Revision};
use buckle_core::error::ErrorReport;
use buckle_core::health::HealthScope;
use buckle_core::plan::RevisionId;
use buckle_core::state::StateStore;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tracing::Instrument;

/// One run a local caller asked for.
#[derive(Debug, Clone)]
pub(super) struct LocalRun {
    pub id: String,
    pub request: Request,
    pub actor: Actor,
    /// Where the caller hears about it. Closed once the caller has left.
    pub events: mpsc::Sender<Event>,
}

/// How many events a caller may leave unread.
const EVENTS: usize = 256;

/// Room kept for what the worker has to say about a run (that it started,
/// its result, how it ended): "waiting behind" notes and an apply's
/// progress never take it.
const RESERVED: usize = 8;

/// The user the agent runs as.
fn agent_uid() -> u32 {
    #[cfg(unix)]
    {
        nix::unistd::geteuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Why `actor` may not ask for `request`, when they may not. Whoever
/// reaches the socket may plan and check health; changing the host takes
/// root (or the user the agent itself runs as, who holds everything the
/// agent does) and a fleet that allows it.
fn refusal(
    request: &Request,
    actor: &Actor,
    agent_uid: u32,
    policy: ApplyPolicy,
) -> Option<ErrorReport> {
    if !matches!(request, Request::Apply { .. }) {
        return None;
    }
    if actor.uid != 0 && actor.uid != agent_uid {
        return Some(
            ErrorReport::user(format!(
                "Only root may apply on this host, and you are {}.",
                actor.user
            ))
            .with_advice(["Run the command with sudo."]),
        );
    }
    policy_refusal(policy)
}

/// Why the fleet's `policy` keeps a host from being changed from its own
/// command line, when it does.
fn policy_refusal(policy: ApplyPolicy) -> Option<ErrorReport> {
    match policy {
        ApplyPolicy::Allowed => None,
        ApplyPolicy::Off => Some(
            ErrorReport::user("This fleet does not allow applies from the host itself.")
                .with_advice([
                    "Roll the change out from the Buckle server.",
                    "To allow it, set `agents.local_runs.apply: on` in the server's configuration.",
                ]),
        ),
        ApplyPolicy::Unknown => Some(
            ErrorReport::user(
                "This host's Buckle server has not said whether hosts may apply from their own command line.",
            )
            .with_advice([
                "Check that the agent has reached its server: `buckle status`.",
                "Upgrade the Buckle server: older servers cannot record an apply made on the host.",
            ]),
        ),
    }
}

impl LocalRun {
    /// How the run reads to someone waiting behind it.
    pub fn describe(&self) -> String {
        format!("local {} by {}", self.request.name(), self.actor.user)
    }

    /// Tells the caller `event`, without ever waiting for it to read: the
    /// worker must not stall behind a caller that stopped listening.
    fn tell(&self, event: Event) {
        if let Err(err) = self.events.try_send(event) {
            tracing::debug!(run = %self.id, error = %err, "a local caller did not hear about its run");
        }
    }

    /// Who ran it, as the server is told.
    pub fn origin(&self) -> LocalOrigin {
        LocalOrigin {
            run_id: self.id.clone(),
            uid: self.actor.uid,
            user: self.actor.user.clone(),
            pid: self.actor.pid,
        }
    }

    /// Whether the run reboots the host when its apply asks for it.
    pub fn reboots(&self) -> bool {
        matches!(self.request, Request::Apply { reboot: true, .. })
    }

    /// Shows the caller the apply events passing through to the returned
    /// channel. A caller that is not reading misses progress, never the
    /// outcome.
    pub fn watch(&self, mut events: mpsc::Receiver<ApplyEvent>) -> mpsc::Receiver<ApplyEvent> {
        let (tx, rx) = mpsc::channel(256);
        let caller = self.events.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if caller.capacity() > RESERVED {
                    let _ = caller.try_send(Event::Apply(Box::new(event.clone())));
                }
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        rx
    }
}

/// What the local socket may ask of the running agent.
#[derive(Clone)]
pub struct LocalControl {
    pub(super) shared: Arc<Shared>,
    pub(super) queue: WeakQueue,
}

impl LocalControl {
    /// Queues `request` behind whatever the agent is doing and returns the
    /// run's events. With `no_wait` a busy agent refuses instead; a caller
    /// who may not ask for `request` is refused at once.
    pub async fn submit(
        &self,
        request: Request,
        actor: Actor,
        no_wait: bool,
    ) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(EVENTS);
        let board = self.shared.board.clone();
        // The channel is new: there is room for a refusal.
        let refuse = |error: ErrorReport| {
            let _ = tx.try_send(Event::Done {
                exit_code: 1,
                error: Some(error),
            });
        };
        let policy = self.shared.local_apply_policy().await;
        if let Some(error) = refusal(&request, &actor, agent_uid(), policy) {
            tracing::warn!(user = %actor.user, uid = actor.uid, what = request.name(), reason = %error.message, "refused a local run");
            refuse(error);
            return rx;
        }
        let Some(queue) = self.queue.upgrade() else {
            refuse(ErrorReport::user("The agent is stopping."));
            return rx;
        };
        if no_wait {
            let (running, queued) = board.snapshot();
            let busy: Vec<String> = running.into_iter().chain(queued).collect();
            if !busy.is_empty() {
                refuse(
                    ErrorReport::user(format!("The agent is busy: {}.", busy.join(", ")))
                        .with_advice(["Run the command without --no-wait to queue behind it."]),
                );
                return rx;
            }
        }
        let run = LocalRun {
            id: format!("local-{:016x}", rand::random::<u64>()),
            request,
            actor,
            events: tx.clone(),
        };
        tracing::info!(run = %run.id, user = %run.actor.user, uid = run.actor.uid, pid = ?run.actor.pid, what = run.request.name(), "a local run was asked for");
        let place = queue.push(Op::Local(run), None);
        // Tells the caller what it waits behind, whenever that changes,
        // until its run starts.
        tokio::spawn(async move {
            let mut moved = board.watch();
            let mut told: Option<Vec<String>> = None;
            let left = loop {
                moved.borrow_and_update();
                match board.ahead_of(place) {
                    None => break false,
                    Some(ahead) if ahead.is_empty() || told.as_ref() == Some(&ahead) => {}
                    // A caller that is not reading hears the next change.
                    Some(_) if tx.capacity() <= RESERVED => {}
                    Some(ahead) => {
                        let event = Event::Queued {
                            ahead: ahead.clone(),
                        };
                        match tx.try_send(event) {
                            Ok(()) => told = Some(ahead),
                            Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break true,
                        }
                    }
                }
                tokio::select! {
                    changed = moved.changed() => if changed.is_err() { break false },
                    _ = tx.closed() => break true,
                }
            };
            // A caller that left no longer stands in line: nobody waits
            // behind it, and the worker skips its run when it comes up.
            if left {
                board.take(place);
            }
        });
        rx
    }

    /// What the agent is doing and the last run it recorded.
    pub async fn status(&self) -> Status {
        let shared = &self.shared;
        let (running, queued) = shared.board.snapshot();
        let store = shared.store.state_store();
        // The engine replaces these files atomically, so a read never sees
        // half of one, whatever is running.
        let (run, outcome) = tokio::task::spawn_blocking(move || {
            let run = store.load().ok().and_then(|mut s| s.runs.pop());
            (run, store.load_last_run().ok().flatten())
        })
        .await
        .unwrap_or_default();
        let data = shared.data.lock().await;
        Status {
            host_id: shared.host_id.clone(),
            agent_version: shared.options.agent_version.clone(),
            server: shared.config.server.clone(),
            connected: shared.connected.load(Ordering::SeqCst),
            host_status: data.agent.status.clone(),
            applied: data.agent.applied_revision.clone(),
            assigned: data.agent.assigned_revision.clone(),
            stable: data.agent.stable_revision.clone(),
            running,
            queued,
            paused_modules: data.agent.paused_modules.clone(),
            rolled_back_modules: data.agent.rolled_back_modules.clone(),
            run,
            outcome,
        }
    }
}

impl Shared {
    /// Whether this host may be changed from its own command line. An
    /// agent with no server answers to nobody else.
    async fn local_apply_policy(&self) -> ApplyPolicy {
        if self.config.is_local() {
            return ApplyPolicy::Allowed;
        }
        self.data.lock().await.agent.local_apply
    }

    /// Runs a local caller's request and tells the caller how it went.
    pub(super) async fn local(&self, run: LocalRun) {
        if run.events.is_closed() {
            tracing::info!(run = %run.id, "skipping a local run whose caller left before it started");
            return;
        }
        let span = tracing::info_span!(
            parent: None,
            "agent.local",
            buckle.host = %self.host_id,
            buckle.revision = tracing::field::Empty,
            buckle.local.run = %run.id,
            buckle.local.command = run.request.name(),
            buckle.local.user = %run.actor.user,
        );
        self.follow_session(&span);
        async {
            let done = match self.local_run(&run).await {
                Ok(exit_code) => Event::Done {
                    exit_code,
                    error: None,
                },
                Err(err) => {
                    tracing::warn!(error = %err.description(), "the local run could not happen");
                    Event::Done {
                        exit_code: 1,
                        error: Some(ErrorReport::of(&err)),
                    }
                }
            };
            run.tell(done);
        }
        .instrument(span)
        .await
    }

    /// The revision `choice` names, with the host's applied and assigned
    /// ones.
    async fn local_revision(
        &self,
        choice: Choice,
    ) -> buckle_core::Result<(RevisionId, Option<RevisionId>, Option<RevisionId>)> {
        let (applied, assigned) = {
            let data = self.data.lock().await;
            (
                data.agent.applied_revision.clone(),
                data.agent.assigned_revision.clone(),
            )
        };
        let chosen = match choice {
            Choice::Applied => applied.clone(),
            Choice::Assigned => assigned.clone().or_else(|| applied.clone()),
        };
        match chosen {
            Some(revision) => Ok((revision, applied, assigned)),
            None if assigned.is_some() => Err(buckle_core::error::user(
                "This host has not applied a revision yet.",
                &["Preview the one its rollout assigned with --revision assigned."],
            )),
            None if self.config.is_local() => Err(buckle_core::error::user(
                "The agent has not applied its source directory yet.",
                &["Check the agent's log for why: `journalctl -u buckle-agent`."],
            )),
            None => Err(buckle_core::error::user(
                "The agent has no revision yet: the server has not assigned it one.",
                &["Roll a revision out to this host from the server, then try again."],
            )),
        }
    }

    /// Carries out the run; the exit code its command ends with.
    async fn local_run(&self, run: &LocalRun) -> buckle_core::Result<i32> {
        let (revision, applied, assigned) = self.local_revision(run.request.revision()).await?;
        tracing::Span::current().record("buckle.revision", revision.0.as_str());
        run.tell(Event::Started {
            run_id: run.id.clone(),
            revision: revision.clone(),
            applied: applied.clone(),
            assigned,
        });
        let rev = self.ensure_revision(&revision, None).await?;
        match &run.request {
            Request::Plan { select, .. } => {
                self.ensure_secrets(&revision, &rev).await;
                let mut ctx = self
                    .context(&revision, select.clone(), Vec::new(), false)
                    .await;
                // The applied revision is planned as the drift pass plans
                // it; a newer one as its rollout would.
                if applied.as_ref() == Some(&revision) {
                    ctx.drift = true;
                    ctx.catch_up = self.data.lock().await.agent.resumed_modules.clone();
                }
                let (plan, facts) = self.runner.plan(rev, &ctx).await?;
                let tree = facts.tree.clone();
                self.remember_facts(facts).await;
                tracing::info!(digest = %plan.digest.short(), changes = plan.changes(), "planned for a local caller");
                let exit_code = if plan.is_empty() { 0 } else { 2 };
                run.tell(Event::Plan {
                    plan: Box::new(plan),
                    facts: tree,
                });
                Ok(exit_code)
            }
            Request::Health { modules, .. } => {
                let ctx = self.context(&revision, None, Vec::new(), false).await;
                let only = (!modules.is_empty()).then_some(modules.as_slice());
                let report = self
                    .runner
                    .health(rev, &ctx, HealthScope::Manual, only)
                    .await?;
                let report = self.scrub_report(&revision, report).await;
                let exit_code = if report.healthy(&[]) { 0 } else { 1 };
                // Every check of the applied revision is a reading of the
                // host's health; some modules, or a revision not applied
                // yet, say nothing about the host as a whole.
                if only.is_none() && applied.as_ref() == Some(&revision) {
                    self.sink
                        .send(
                            Report::Health {
                                request_id: None,
                                revision: Some(revision.clone()),
                                report: report.clone(),
                            }
                            .from_local(Some(&run.origin())),
                        )
                        .await;
                }
                run.tell(Event::Health(report));
                Ok(exit_code)
            }
            Request::Apply {
                select,
                waive,
                include_rolled_back,
                ..
            } => {
                self.local_apply(
                    run,
                    &revision,
                    rev,
                    select.clone(),
                    waive.clone(),
                    *include_rolled_back,
                )
                .await
            }
        }
    }

    /// Re-applies the applied revision for a local caller, as the drift
    /// pass does unattended: uncertain selections are held and removals
    /// never run. Reported to the server with who ran it.
    async fn local_apply(
        &self,
        run: &LocalRun,
        revision: &RevisionId,
        rev: Arc<Revision>,
        select: Option<String>,
        waive: Vec<String>,
        include_rolled_back: bool,
    ) -> buckle_core::Result<i32> {
        // The fleet's answer may have changed while the run waited.
        if let Some(error) = policy_refusal(self.local_apply_policy().await) {
            return Err(buckle_core::error::user(
                error.message,
                &["Roll the change out from the Buckle server."],
            ));
        }
        let (paused, rolled_back, resumed) = {
            let data = self.data.lock().await;
            (
                data.agent.status.as_deref() == Some("paused"),
                data.agent.rolled_back_modules.clone(),
                data.agent.resumed_modules.clone(),
            )
        };
        if paused {
            return Err(buckle_core::error::user(
                "This host is paused on the Buckle server: nothing is applied to it.",
                &["Resume the host from the server's hosts page, then try again."],
            ));
        }
        if !rolled_back.is_empty() && !include_rolled_back {
            return Err(buckle_core::error::user(
                format!(
                    "Modules of this revision were rolled back after a failure ({}) and wait for the server.",
                    rolled_back.join(", ")
                ),
                &[
                    "To apply them again anyway, pass --include-rolled-back.",
                    "Fix the revision and roll it out from the Buckle server.",
                ],
            ));
        }
        self.ensure_secrets(revision, &rev).await;
        let whole = select.is_none();
        let mut ctx = self.context(revision, select, waive, false).await;
        ctx.drift = true;
        let catching_up = !resumed.is_empty();
        ctx.catch_up = resumed;
        let (plan, facts) = self.runner.plan(rev.clone(), &ctx).await?;
        let tree = facts.tree.clone();
        self.remember_facts(facts).await;
        run.tell(Event::Plan {
            plan: Box::new(plan.clone()),
            facts: tree,
        });
        if plan.is_empty() && !catching_up {
            tracing::info!("a local apply found nothing to change");
            if whole {
                // The server hears that the host is as configured.
                self.sink
                    .send(Report::Drift {
                        revision: revision.clone(),
                        plan: None,
                        corrected: false,
                    })
                    .await;
            }
            return Ok(0);
        }
        tracing::info!(digest = %plan.digest.short(), changes = plan.changes(), "applying for a local caller");
        let outcome = self
            .run_apply_for(rev, &ctx, &plan, true, true, Some(run))
            .await;
        if whole {
            // As after a drift pass: what was found, and whether it is gone.
            self.sink
                .send(Report::Drift {
                    revision: revision.clone(),
                    corrected: !plan.is_empty() && outcome.succeeded,
                    plan: (!plan.is_empty()).then_some(plan),
                })
                .await;
        }
        let exit_code = if outcome.succeeded { 0 } else { 1 };
        let rebooting = outcome.reboot.is_some() && run.reboots();
        run.tell(Event::Outcome {
            outcome: Box::new(outcome),
            rebooting,
        });
        Ok(exit_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const AGENT: u32 = 990;

    fn apply() -> Request {
        Request::Apply {
            select: None,
            waive: Vec::new(),
            include_rolled_back: false,
            reboot: false,
        }
    }

    fn plan() -> Request {
        Request::Plan {
            revision: Choice::Applied,
            select: None,
        }
    }

    #[rstest]
    #[case::anyone_plans(plan(), 1000, ApplyPolicy::Allowed, None)]
    #[case::planning_ignores_the_policy(plan(), 1000, ApplyPolicy::Off, None)]
    #[case::root_applies(apply(), 0, ApplyPolicy::Allowed, None)]
    #[case::the_agent_s_user_applies(apply(), AGENT, ApplyPolicy::Allowed, None)]
    #[case::others_do_not(apply(), 1000, ApplyPolicy::Allowed, Some("Only root"))]
    #[case::nobody_does_when_the_fleet_says_no(
        apply(),
        0,
        ApplyPolicy::Off,
        Some("does not allow")
    )]
    #[case::nor_before_the_server_has_said(apply(), 0, ApplyPolicy::Unknown, Some("has not said"))]
    fn changing_the_host_takes_root_and_a_fleet_that_allows_it(
        #[case] request: Request,
        #[case] uid: u32,
        #[case] policy: ApplyPolicy,
        #[case] refused: Option<&str>,
    ) {
        let actor = Actor {
            uid,
            user: "ben".into(),
            pid: None,
        };
        let said = refusal(&request, &actor, AGENT, policy);
        match refused {
            None => assert!(said.is_none(), "{said:?}"),
            Some(why) => {
                let said = said.expect("refused");
                assert!(said.message.contains(why), "{said:?}");
                assert!(!said.advice.is_empty(), "the caller is told what to do");
            }
        }
    }
}
