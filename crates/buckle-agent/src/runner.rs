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
use buckle_core::selectors::{HostIdentity, Labels};
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
}

/// The production runner: [`Engine`] against the real host.
pub struct EngineRunner {
    system: Arc<dyn System>,
    state: Arc<dyn StateStore>,
    parallelism: usize,
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
        }
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
        Ok(Inputs {
            revision,
            host: ctx.host.clone(),
            server_labels: ctx.server_labels.clone(),
            system: self.system.clone(),
            secrets: ctx.secrets.clone(),
            state: self.state.clone(),
            facts: ctx.facts.clone(),
            select,
            waived_checks: ctx.waived_checks.clone(),
            parallelism: self.parallelism,
        })
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
        Engine::plan(&self.inputs(revision, ctx)?).await
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
}
