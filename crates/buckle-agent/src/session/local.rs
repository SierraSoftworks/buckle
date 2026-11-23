//! Local runs: what a caller on the agent's socket asked for, run as an
//! operation on the host's worker like any other.

// Nothing serves the socket on Windows yet, so nothing submits there.
#![cfg_attr(not(unix), allow(dead_code))]

use super::{Op, Shared, WeakQueue};
use crate::local::{Actor, Choice, Event, Request, Status};
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
const EVENTS: usize = 64;

/// Room kept for what the worker has to say about a run (that it started,
/// its result, how it ended): "waiting behind" notes never take it.
const RESERVED: usize = 8;

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
}

/// What the local socket may ask of the running agent.
#[derive(Clone)]
pub struct LocalControl {
    pub(super) shared: Arc<Shared>,
    pub(super) queue: WeakQueue,
}

impl LocalControl {
    /// Queues `request` behind whatever the agent is doing and returns the
    /// run's events. With `no_wait` a busy agent refuses instead.
    pub fn submit(&self, request: Request, actor: Actor, no_wait: bool) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(EVENTS);
        let board = self.shared.board.clone();
        // The channel is new: there is room for a refusal.
        let refuse = |error: ErrorReport| {
            let _ = tx.try_send(Event::Done {
                exit_code: 1,
                error: Some(error),
            });
        };
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
                let modules = (!modules.is_empty()).then_some(modules.as_slice());
                let report = self
                    .runner
                    .health(rev, &ctx, HealthScope::Manual, modules)
                    .await?;
                let report = self.scrub_report(&revision, report).await;
                let exit_code = if report.healthy(&[]) { 0 } else { 1 };
                run.tell(Event::Health(report));
                Ok(exit_code)
            }
        }
    }
}
