//! The engine: load a revision, collect facts, select modules, resolve vars and
//! secrets, plan, apply, remove, and run health checks.
//!
//! Lane 1 (core engine) owns this module. The skeleton fixes the public API so
//! that the agent, the CLI and the server compile against it, and provides
//! placeholder implementations that return a clear "not implemented" error.

pub mod loader;

use crate::facts::Facts;
use crate::health::{HealthReport, HealthScope};
use crate::model::FleetConfig;
use crate::plan::{Plan, RevisionId};
use crate::secrets::SecretStore;
use crate::selectors::{HostIdentity, Labels};
use crate::state::StateStore;
use crate::system::System;
use crate::task::Source;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// A loaded revision: the fleet config plus every module and its files.
#[derive(Debug)]
pub struct Revision {
    pub id: RevisionId,
    pub fleet: FleetConfig,
    pub modules: Vec<LoadedModule>,
    /// Raw contents of every `vars` overlay file referenced by the fleet config.
    pub var_files: indexmap::IndexMap<PathBuf, serde_yaml::Value>,
}

impl std::fmt::Debug for LoadedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedModule")
            .field("module", &self.config.name)
            .field("templates", &self.templates.len())
            .finish()
    }
}

pub struct LoadedModule {
    pub config: crate::model::ModuleConfig,
    pub source: Arc<dyn Source>,
    /// Templates under `files/` keyed by relative path, for includes.
    pub templates: Vec<(PathBuf, String)>,
}

/// Inputs to a plan or apply for one host.
pub struct Inputs {
    pub revision: Arc<Revision>,
    pub host: HostIdentity,
    /// Labels managed on the server, overlaid on the agent's own.
    pub server_labels: Labels,
    pub system: Arc<dyn System>,
    pub secrets: Arc<SecretStore>,
    pub state: Arc<dyn StateStore>,
    /// Previously collected facts, refreshed per provider TTL.
    pub facts: Facts,
    /// Restrict to tasks matching this selector (e.g. `task.tags contains "network"`).
    pub select: Option<crate::Filter>,
    /// Checks waived for this run: `module/check`.
    pub waived_checks: Vec<String>,
    pub parallelism: usize,
}

/// Result of an apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub plan_digest: crate::Digest,
    pub succeeded: bool,
    pub changes: usize,
    pub removals: usize,
    pub duration_ms: u64,
    pub module_results: Vec<ModuleOutcome>,
    /// The module whose failure halted the run, if any.
    pub failed_module: Option<String>,
    /// Modules re-applied from the stable revision because they failed.
    pub rolled_back_modules: Vec<String>,
    /// Modules not started because the run halted.
    pub not_started: Vec<String>,
    pub health: Option<HealthReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleOutcome {
    pub module: String,
    pub status: ModuleStatus,
    pub changes: usize,
    pub duration_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleStatus {
    Applied,
    Unchanged,
    Failed,
    Unhealthy,
    RolledBack,
    NotStarted,
    Deferred,
}

/// Progress events emitted during apply, for the agent to stream to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ApplyEvent {
    ModuleStarted {
        module: String,
    },
    StepStarted {
        module: String,
        task_id: String,
        kind: String,
    },
    StepFinished {
        module: String,
        task_id: String,
        kind: String,
        changed: bool,
        summary: String,
    },
    StepFailed {
        module: String,
        task_id: String,
        kind: String,
        error: String,
    },
    HandlerRun {
        module: String,
        handler: String,
    },
    ModuleFinished {
        module: String,
        status: ModuleStatus,
    },
    RemovalRun {
        resource: String,
        summary: String,
    },
    HealthChecked {
        module: String,
        check: String,
        passed: bool,
        reason: Option<String>,
    },
    FactsRefreshed {
        providers: Vec<String>,
    },
}

/// The engine entry points. Lane 1 implements these.
pub struct Engine;

impl Engine {
    /// Pure: collects facts (respecting TTLs), selects modules, resolves vars,
    /// plans every task and removal. Never mutates the host.
    pub async fn plan(inputs: &Inputs) -> crate::Result<(Plan, Facts)> {
        let _ = inputs;
        Err(not_implemented("Engine::plan"))
    }

    /// Applies a previously produced plan. Refuses when the host's fresh plan
    /// digest no longer matches (stale plan).
    pub async fn apply(
        inputs: &Inputs,
        plan: &Plan,
        events: tokio::sync::mpsc::Sender<ApplyEvent>,
    ) -> crate::Result<ApplyOutcome> {
        let _ = (inputs, plan, events);
        Err(not_implemented("Engine::apply"))
    }

    /// Runs the health checks of the given modules (or all applied modules when `None`).
    pub async fn health(
        inputs: &Inputs,
        scope: HealthScope,
        modules: Option<&[String]>,
    ) -> crate::Result<HealthReport> {
        let _ = (inputs, scope, modules);
        Err(not_implemented("Engine::health"))
    }
}

fn not_implemented(what: &str) -> crate::Error {
    crate::error::system(
        format!("{what} is not implemented yet."),
        &["This part of Buckle is still being built."],
    )
}
