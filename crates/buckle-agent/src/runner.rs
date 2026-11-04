//! The seam between the agent and the engine. Everything the agent does to
//! the host goes through a [`Runner`]: [`EngineRunner`] drives
//! [`buckle_core::engine::Engine`]; tests use the scripted
//! [`crate::testing::FakeRunner`].

use async_trait::async_trait;
use buckle_core::engine::{ApplyEvent, ApplyOutcome, Engine, Inputs, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::{HealthReport, HealthScope};
use buckle_core::plan::Plan;
use buckle_core::secrets::SecretStore;
use buckle_core::selectors::{HostIdentity, Labels, Vars};
use buckle_core::state::StateStore;
use buckle_core::system::System;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Per-run inputs the agent supplies alongside a revision.
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    /// The host identity with its seed labels.
    pub host: HostIdentity,
    /// Effective labels from the server, overlaid on the seed labels.
    pub server_labels: Labels,
    /// Secrets scoped to this host for the revision.
    pub secrets: Arc<SecretStore>,
    /// The facts collected so far (providers refresh per TTL).
    pub facts: Facts,
    /// Restrict the plan to tasks matching this filt-rs selector.
    pub select: Option<String>,
    /// Checks waived for this run (`module/check`).
    pub waived_checks: Vec<String>,
    /// The operator forced the run despite a failing baseline.
    pub force: bool,
    /// The cached stable revision the engine re-applies module by module when
    /// a module fails (design §04). `None` disables module-scoped rollback.
    pub stable_revision: Option<Arc<Revision>>,
    /// Agent-local var overrides from `agent.yaml`, the last vars layer (design §03).
    pub vars: Vars,
    /// A drift pass: plan (and correct) in drift mode, never running
    /// removals (an unattended pass must not remove on an uncertain
    /// selection; held modules are listed in `plan.held`).
    pub drift: bool,
}

/// Plans, applies, checks health and collects facts for the agent.
#[async_trait]
pub trait Runner: Send + Sync + 'static {
    /// Collects (or refreshes, per TTL) the facts about this host.
    async fn facts(&self, previous: &Facts) -> buckle_core::Result<Facts>;

    /// Plans `revision` for this host. Pure: never mutates the host.
    async fn plan(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
    ) -> buckle_core::Result<(Plan, Facts)>;

    /// Applies `plan`, streaming progress to `events`.
    async fn apply(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> buckle_core::Result<ApplyOutcome>;

    /// Runs the health checks of `modules` (or every applied module).
    async fn health(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> buckle_core::Result<HealthReport>;

    /// The identity of the current boot, when the host exposes one.
    async fn boot_id(&self) -> Option<String> {
        None
    }

    /// Reboots the host, as a run asked for. Returns once the reboot has been
    /// started (the agent is normally stopped shortly after).
    async fn reboot(&self) -> buckle_core::Result<()> {
        Err(buckle_core::error::user(
            "This agent cannot reboot the host.",
            &["Reboot the host by hand."],
        ))
    }
}

/// The production runner: [`Engine`] against the real host.
pub struct EngineRunner {
    system: Arc<dyn System>,
    state: Arc<dyn StateStore>,
    parallelism: usize,
    scratch_dir: Option<std::path::PathBuf>,
}

impl EngineRunner {
    /// A runner over `system`, persisting resources in `state`.
    pub fn new(system: Arc<dyn System>, state: Arc<dyn StateStore>) -> Self {
        Self {
            system,
            state,
            parallelism: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            scratch_dir: None,
        }
    }

    /// The directory planning may write to (`<state_dir>/tmp`); every other
    /// write in plan mode is refused by the engine's plan guard.
    pub fn with_scratch_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.scratch_dir = Some(dir.into());
        self
    }

    fn inputs(&self, revision: Arc<Revision>, ctx: &RunContext) -> buckle_core::Result<Inputs> {
        let select = match &ctx.select {
            Some(text) => Some(buckle_core::Filter::new(text.clone()).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("The task selector '{text}' is not a valid filt-rs expression."),
                    &["Check the selector sent with the plan request, e.g. 'task.tags contains \"network\"'."],
                )
            })?),
            None => None,
        };
        let mut inputs = Inputs::new(
            revision,
            ctx.host.clone(),
            self.system.clone(),
            ctx.secrets.clone(),
            self.state.clone(),
        );
        if let Some(dir) = &self.scratch_dir {
            inputs = inputs.with_scratch_dir(dir.clone());
        }
        Ok(inputs
            .with_server_labels(ctx.server_labels.clone())
            .with_facts(ctx.facts.clone())
            .with_select(select)
            .with_waived_checks(ctx.waived_checks.clone())
            .with_parallelism(self.parallelism)
            .with_stable_revision(ctx.stable_revision.clone())
            .with_agent_vars(ctx.vars.clone())
            .with_drift(ctx.drift)
            .with_removals(!ctx.drift))
    }
}

#[async_trait]
impl Runner for EngineRunner {
    async fn facts(&self, previous: &Facts) -> buckle_core::Result<Facts> {
        let cx = buckle_core::task::Cx {
            mode: buckle_core::task::Mode::Plan,
            system: self.system.clone(),
            host: Arc::new(buckle_core::selectors::HostContext::default()),
            templater: Arc::new(buckle_core::template::Templater::new()),
            source: Arc::new(buckle_core::task::DirSource::new(std::env::temp_dir())),
            module: String::new(),
            secrets: Arc::new(SecretStore::new()),
            waived_checks: Arc::new(Vec::new()),
            previous: None,
        };
        let mut facts = previous.clone();
        if facts.tree.is_null() {
            facts = Facts::new();
        }
        for error in buckle_core::facts::collect(&cx, &mut facts, None, false).await {
            tracing::debug!(provider = %error.provider, error = %error.message, "fact provider failed");
        }
        Ok(facts)
    }

    async fn plan(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
    ) -> buckle_core::Result<(Plan, Facts)> {
        let inputs = self.inputs(revision, ctx)?;
        if ctx.drift {
            Engine::drift(&inputs).await
        } else {
            Engine::plan(&inputs).await
        }
    }

    async fn apply(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        plan: &Plan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> buckle_core::Result<ApplyOutcome> {
        Engine::apply(&self.inputs(revision, ctx)?, plan, events).await
    }

    async fn health(
        &self,
        revision: Arc<Revision>,
        ctx: &RunContext,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> buckle_core::Result<HealthReport> {
        Engine::health(&self.inputs(revision, ctx)?, scope, modules).await
    }

    async fn boot_id(&self) -> Option<String> {
        buckle_core::reboot::boot_id(self.system.as_ref()).await
    }

    async fn reboot(&self) -> buckle_core::Result<()> {
        buckle_core::reboot::reboot(self.system.as_ref()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_runs_never_remove_and_planning_may_only_write_the_scratch_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").expect("write");
        let revision = Arc::new(
            buckle_core::engine::loader::load_dir(
                dir.path(),
                buckle_core::plan::RevisionId::new("r"),
            )
            .expect("loads"),
        );
        let runner = EngineRunner::new(
            Arc::new(buckle_core::system::RealSystem::new()),
            Arc::new(buckle_core::state::MemoryStateStore::default()),
        )
        .with_scratch_dir(dir.path().join("tmp"));
        let drift = runner
            .inputs(
                revision.clone(),
                &RunContext {
                    drift: true,
                    ..Default::default()
                },
            )
            .expect("inputs");
        assert!(drift.drift);
        assert!(!drift.removals, "drift correction never removes");
        assert_eq!(drift.scratch_dirs, vec![dir.path().join("tmp")]);
        let assigned = runner
            .inputs(revision, &RunContext::default())
            .expect("inputs");
        assert!(!assigned.drift);
        assert!(assigned.removals);
        assert_eq!(assigned.scratch_dirs, vec![dir.path().join("tmp")]);
    }
}
