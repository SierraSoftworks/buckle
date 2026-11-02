//! The pure half of the engine: facts, groups, vars, module selection and
//! ordering, task preparation and planning, handler collection, deferral and
//! removals. Nothing here writes to the host.

use super::context;
use super::graph::Graph;
use super::{Inputs, LoadedModule, Revision};
use crate::facts::Facts;
use crate::model::{ModuleConfig, RemovalPolicy, RetryPolicy, TaskDecl};
use crate::plan::{
    Deferred, HandlerRun, Held, Indeterminate, ModulePlan, Plan, Planned, Rehome, RehomeKind,
    Removal, ResourceKey, Step, TaskRef,
};
use crate::secrets::SecretStore;
use crate::selectors::{HostContext, HostIdentity, Vars};
use crate::state::{ResourceRecord, State};
use crate::system::System;
use crate::task::{Cx, DynTask, EmptySource, ErasedSpec, Mode, Source};
use crate::template::Templater;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use tracing::Instrument;

/// Registers every module's `files/*.j2` under `<module>/<relpath>` so that
/// includes resolve within (and across) modules.
pub(super) fn build_templater(revision: &Revision) -> crate::Result<Templater> {
    let mut templater = Templater::new();
    for module in &revision.modules {
        for (path, source) in &module.templates {
            let name = format!(
                "{}/{}",
                module.config.name,
                path.to_string_lossy().replace('\\', "/")
            );
            templater
                .add_template(name.clone(), source.clone())
                .map_err(|e| {
                    crate::error::wrap_user(
                        e,
                        format!(
                            "The template {name} in module '{}' does not compile.",
                            module.config.name
                        ),
                        &["Fix the template syntax; the internal error names the line."],
                    )
                })?;
        }
    }
    Ok(templater)
}

/// Base contexts for one facts snapshot, with per-module contexts (vars
/// resolved) computed on demand and cached.
pub(super) struct Contexts {
    pub base: Arc<HostContext>,
    cache: Mutex<HashMap<String, Arc<HostContext>>>,
    /// The snapshot the contexts were built from (for unavailable providers).
    facts: Arc<Facts>,
    /// Groups whose membership could not be decided (see
    /// [`context::evaluate_groups_checked`]).
    uncertain_groups: Vec<String>,
}

/// Whether a selector selects, given what is known about the host.
pub(super) enum Decision {
    Yes,
    No,
    /// The selector cannot be decided; the reason says why.
    Unknown(String),
    /// The module is selected but its vars could not be rendered.
    Broken(String),
}

impl Contexts {
    /// Why `selector` cannot be trusted on this host, if it cannot.
    pub fn uncertainty(&self, selector: &crate::Filter) -> Option<String> {
        let raw = selector.raw();
        let groups: Vec<String> = self
            .uncertain_groups
            .iter()
            .filter(|g| {
                regex::Regex::new(&format!(r"\b{}\b", regex::escape(g)))
                    .map(|re| re.is_match(raw))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        context::selector_uncertainty(raw, &self.facts, &groups)
    }

    /// Decides a `when` (absent means yes) in `ctx`, refusing to guess.
    pub fn decide(&self, when: Option<&crate::Filter>, ctx: &HostContext) -> Decision {
        let Some(when) = when else {
            return Decision::Yes;
        };
        if let Some(reason) = self.uncertainty(when) {
            return Decision::Unknown(format!("its `when` cannot be decided: {reason}"));
        }
        match ctx.try_matches(when) {
            Ok(true) => Decision::Yes,
            Ok(false) => Decision::No,
            Err(e) => Decision::Unknown(e),
        }
    }
}

/// A task after rendering and parsing but before `plan`.
pub(super) struct PreparedTask {
    pub decl: TaskDecl,
    pub task: &'static dyn DynTask,
    pub spec: ErasedSpec,
    pub key: ResourceKey,
    /// Task-scoped context in plan mode.
    pub cx: Cx,
    /// Whether the task passes `Inputs.select`. Unselected tasks still count
    /// towards the desired set so their resources are not treated as orphans.
    pub selected: bool,
}

pub(super) struct PreparedModule {
    pub name: String,
    pub needs: Vec<String>,
    pub ctx: Arc<HostContext>,
    pub source: Arc<dyn Source>,
    pub tasks: Vec<PreparedTask>,
    pub provides: Vec<String>,
    /// Tasks whose `when` could not be decided (kept, not planned).
    pub warnings: Vec<String>,
    /// The module's `removal.policy`, if it sets one.
    pub removal: Option<RemovalPolicy>,
}

impl PreparedModule {
    pub fn desired_keys(&self) -> impl Iterator<Item = &ResourceKey> {
        self.tasks.iter().map(|t| &t.key)
    }
}

/// What the executor needs for a step beyond the serialisable [`Step`].
pub(super) struct RichStep {
    pub task: &'static dyn DynTask,
    pub spec: ErasedSpec,
    pub cx: Cx,
    pub retry: RetryPolicy,
    /// Blocked by the engine because the kind does not support this platform
    /// (never planned again), as opposed to blocked by the task itself.
    pub platform_blocked: bool,
}

pub(super) struct RichHandler {
    pub task: &'static dyn DynTask,
    pub spec: ErasedSpec,
    pub cx: Cx,
    pub planned: Planned,
    pub retry: RetryPolicy,
}

pub(super) struct RichModulePlan {
    pub plan: ModulePlan,
    /// Parallel to `plan.steps`.
    pub steps: Vec<RichStep>,
    /// Parallel to `plan.handlers`.
    pub handlers: Vec<RichHandler>,
    pub ctx: Arc<HostContext>,
    pub source: Arc<dyn Source>,
    /// Resources of tasks not planned (excluded by `--select`, or with an
    /// undecidable `when`) that stay in the desired set.
    pub unselected: Vec<ResourceKey>,
}

pub(super) struct RichRemoval {
    pub record: ResourceRecord,
    pub task: Option<&'static dyn DynTask>,
}

pub(super) struct RichPlan {
    pub plan: Plan,
    /// Parallel to `plan.modules`.
    pub modules: Vec<RichModulePlan>,
    /// Parallel to `plan.removals`.
    pub removals: Vec<RichRemoval>,
    /// Every selected module (planned or deferred) in dependency order.
    pub order: Vec<String>,
    /// What each module in `order` waits for before it may start: its
    /// `needs`, plus the provider modules a deferred module waits on.
    pub needs: HashMap<String, Vec<String>>,
    /// The desired resource keys of the planned modules.
    pub desired: Vec<ResourceKey>,
}

/// What a deferred module turned out to be once planned at apply time.
pub(super) enum DeferredPlan {
    Planned(RichModulePlan),
    /// Its `when` is false against the refreshed facts.
    Unselected,
    Indeterminate(String),
}

/// Module selection for a host.
pub(super) struct Selection<'a> {
    /// Selected modules in dependency order.
    pub order: Vec<&'a LoadedModule>,
    pub skipped: Vec<String>,
    /// Modules whose selection could not be decided, sorted by name.
    pub indeterminate: Vec<Indeterminate>,
    /// Selected modules (in `order`) whose vars could not be rendered, with
    /// the reason: a plan error for the module, unless the vars read facts a
    /// provider module will produce in this run (then it is deferred).
    pub broken: HashMap<String, String>,
    /// Modules whose `when` is false but reads facts that selected modules
    /// provide, with those provider modules: decided after they apply when
    /// they change something, else skipped.
    pub conditional: Vec<(&'a LoadedModule, Vec<String>)>,
}

/// Plans one revision for one host. Built from [`Inputs`] (or, for rollback,
/// from the inputs with the stable revision substituted).
pub(super) struct Planner<'a> {
    pub revision: &'a Revision,
    pub host: HostIdentity,
    pub system: Arc<dyn System>,
    pub secrets: Arc<SecretStore>,
    pub select: Option<&'a crate::Filter>,
    pub waived: Arc<Vec<String>>,
    pub agent_vars: &'a Vars,
    pub templater: Arc<Templater>,
    /// Drift mode (see [`Inputs::drift`]).
    pub drift: bool,
    /// JSON schemas by task kind, for secret-field aware spec rendering.
    schemas: Mutex<HashMap<&'static str, Arc<serde_json::Value>>>,
}

impl<'a> Planner<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        revision: &'a Revision,
        host: HostIdentity,
        system: Arc<dyn System>,
        secrets: Arc<SecretStore>,
        select: Option<&'a crate::Filter>,
        waived: Arc<Vec<String>>,
        agent_vars: &'a Vars,
    ) -> crate::Result<Self> {
        Ok(Self {
            revision,
            host,
            system,
            secrets,
            select,
            waived,
            agent_vars,
            templater: Arc::new(build_templater(revision)?),
            drift: false,
            schemas: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_inputs(inputs: &'a Inputs, revision: &'a Revision) -> crate::Result<Self> {
        let mut planner = Self::new(
            revision,
            HostIdentity {
                id: inputs.host.id.clone(),
                labels: inputs.labels(),
            },
            inputs.system.clone(),
            inputs.secrets.clone(),
            inputs.select.as_ref(),
            Arc::new(inputs.waived_checks.clone()),
            &inputs.agent_vars,
        )?;
        planner.drift = inputs.drift;
        Ok(planner)
    }

    fn schema_of(&self, task: &'static dyn DynTask) -> Arc<serde_json::Value> {
        if let Some(s) = self
            .schemas
            .lock()
            .ok()
            .and_then(|c| c.get(task.kind()).cloned())
        {
            return s;
        }
        let schema = Arc::new(serde_json::to_value(task.schema()).unwrap_or_default());
        if let Ok(mut cache) = self.schemas.lock() {
            cache.insert(task.kind(), schema.clone());
        }
        schema
    }

    /// Renders the template strings of a task spec with [`Templater::render_plain`],
    /// except fields the kind's schema marks as secret-capable
    /// (`x-buckle-secret`), which keep their raw template so the task can
    /// resolve them through a sink.
    pub fn render_spec(
        &self,
        task: &'static dyn DynTask,
        spec: &serde_yaml::Value,
        ctx: &HostContext,
    ) -> crate::Result<serde_json::Value> {
        let mut json = context::yaml_to_json(spec);
        let schema = self.schema_of(task);
        crate::secrets::field::render_spec_strings(&mut json, &schema, &mut |s| {
            self.templater.render_plain(s, ctx)
        })?;
        Ok(json)
    }

    pub fn cx(&self, module: &str, source: Arc<dyn Source>, host: HostContext, mode: Mode) -> Cx {
        Cx {
            mode,
            system: self.system.clone(),
            host: Arc::new(host),
            templater: self.templater.clone(),
            source,
            module: module.to_owned(),
            secrets: self.secrets.clone(),
            waived_checks: self.waived.clone(),
            previous: None,
        }
    }

    /// A context for code that runs outside any module (fact providers).
    pub fn facts_cx(&self) -> Cx {
        let host = HostContext::new(self.host.clone()).with_secrets(self.secrets.clone());
        self.cx("facts", Arc::new(EmptySource), host, Mode::Plan)
    }

    /// A context for removing `record`, whose module may no longer exist.
    pub fn removal_cx(&self, contexts: &Contexts, record: &ResourceRecord) -> Cx {
        let module = record.key.module.as_str();
        let source: Arc<dyn Source> = self
            .revision
            .module(module)
            .map(|m| m.source.clone())
            .unwrap_or_else(|| Arc::new(EmptySource));
        let host = contexts.base.in_module(module, &[]);
        let mut cx = self.cx(module, source, host, Mode::Apply);
        cx.previous = Some(Arc::new(record.clone()));
        cx
    }

    /// Labels, groups, facts and secrets for one facts snapshot.
    pub fn contexts(&self, facts: &Facts) -> Contexts {
        let base = HostContext::new(self.host.clone())
            .with_facts(facts.tree.clone())
            .with_secrets(self.secrets.clone());
        let (groups, uncertain_groups) =
            context::evaluate_groups_checked(self.revision, &base, facts);
        Contexts {
            base: Arc::new(base.with_groups(groups)),
            cache: Mutex::new(HashMap::new()),
            facts: Arc::new(facts.clone()),
            uncertain_groups,
        }
    }

    /// The module-scoped context with vars resolved (cached per facts snapshot).
    pub fn module_context(
        &self,
        contexts: &Contexts,
        module: &ModuleConfig,
    ) -> crate::Result<Arc<HostContext>> {
        if let Some(ctx) = contexts
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(&module.name).cloned())
        {
            return Ok(ctx);
        }
        let scoped = contexts.base.in_module(&module.name, &module.tags);
        let vars = context::resolve_vars(
            self.revision,
            module,
            self.agent_vars,
            &self.templater,
            &scoped,
        )?;
        let ctx = Arc::new(scoped.with_vars(vars));
        if let Ok(mut cache) = contexts.cache.lock() {
            cache.insert(module.name.clone(), ctx.clone());
        }
        Ok(ctx)
    }

    /// Hides secret values in text that ends up in a plan.
    pub fn scrub(&self, text: &str) -> String {
        let redactor = crate::secrets::redact::Redactor::new(&self.secrets);
        if redactor.is_empty() {
            text.to_owned()
        } else {
            redactor.redact(text).into_inner()
        }
    }

    /// A module-scoped failure for a plan: the scrubbed message and advice.
    pub fn failure(&self, err: &crate::Error) -> String {
        let advice = err.advice();
        if advice.is_empty() {
            self.scrub(&err.message())
        } else {
            self.scrub(&format!("{} ({})", err.message(), advice.join(" ")))
        }
    }

    /// Evaluates every module's `when`, checks that selected modules only need
    /// selected modules, and orders them along the DAG. A module whose
    /// selection cannot be decided (its `when` errs or reads unavailable facts,
    /// its vars fail to render, its directory has no module.yaml, or it needs
    /// such a module) is indeterminate, never "unselected".
    pub fn select(&self, contexts: &Contexts) -> crate::Result<Selection<'a>> {
        let mut selected: HashMap<&str, &'a LoadedModule> = HashMap::new();
        let mut skipped = Vec::new();
        let mut broken: HashMap<String, String> = HashMap::new();
        let mut unselected: Vec<&'a LoadedModule> = Vec::new();
        let mut indeterminate: Vec<Indeterminate> = self
            .revision
            .unloaded
            .iter()
            .map(|name| Indeterminate {
                module: name.clone(),
                reason: format!("the directory modules/{name} has no module.yaml"),
                plan_error: false,
            })
            .collect();
        for module in &self.revision.modules {
            let cfg = &module.config;
            let decision = match self.module_context(contexts, cfg) {
                Ok(ctx) => contexts.decide(cfg.when.as_ref(), &ctx),
                Err(err) => {
                    let reason = format!(
                        "its vars could not be rendered: {}",
                        self.scrub(&err.message())
                    );
                    match &cfg.when {
                        Some(w) if context::reads_vars(w.raw()) => {
                            Decision::Unknown(format!("its `when` reads vars, and {reason}"))
                        }
                        when => {
                            let bare = contexts.base.in_module(&cfg.name, &cfg.tags);
                            match contexts.decide(when.as_ref(), &bare) {
                                Decision::Yes => Decision::Broken(reason),
                                other => other,
                            }
                        }
                    }
                }
            };
            match decision {
                Decision::Yes => {
                    selected.insert(cfg.name.as_str(), module);
                }
                Decision::No => unselected.push(module),
                Decision::Unknown(reason) => indeterminate.push(Indeterminate {
                    module: cfg.name.clone(),
                    reason,
                    plan_error: false,
                }),
                Decision::Broken(reason) => {
                    selected.insert(cfg.name.as_str(), module);
                    broken.insert(cfg.name.clone(), reason);
                }
            }
        }
        // A `when` that reads facts a selected module provides may change once
        // that module applies; such modules are decided by the planner.
        let mut conditional = Vec::new();
        for module in unselected {
            let refs = module
                .config
                .when
                .as_ref()
                .map(|w| context::fact_references(w.raw()))
                .unwrap_or_default();
            let mut providers: Vec<String> = selected
                .values()
                .filter(|p| {
                    !p.config.facts.provides.is_empty()
                        && refs
                            .iter()
                            .any(|r| context::provided_by(r, &p.config.facts.provides))
                })
                .map(|p| p.config.name.clone())
                .collect();
            providers.sort();
            if providers.is_empty() {
                skipped.push(module.config.name.clone());
            } else {
                conditional.push((module, providers));
            }
        }
        // A module that needs an indeterminate module cannot run either.
        loop {
            let mut blocked: Vec<(&str, String)> = selected
                .iter()
                .filter_map(|(name, m)| {
                    m.config.needs.iter().find_map(|need| {
                        indeterminate.iter().find(|i| &i.module == need).map(|i| {
                            let what = if i.plan_error {
                                "could not be planned"
                            } else {
                                "is indeterminate"
                            };
                            (*name, format!("it needs '{need}', which {what}"))
                        })
                    })
                })
                .collect();
            if blocked.is_empty() {
                break;
            }
            blocked.sort();
            for (name, reason) in blocked {
                selected.remove(name);
                indeterminate.push(Indeterminate {
                    module: name.to_owned(),
                    reason,
                    plan_error: false,
                });
            }
        }
        indeterminate.sort_by(|a, b| a.module.cmp(&b.module));
        let mut graph = Graph::new();
        for (name, module) in &selected {
            for need in &module.config.needs {
                if !selected.contains_key(need.as_str()) {
                    let reason = if self.revision.module(need).is_some() {
                        format!("'{need}' is not selected on this host")
                    } else {
                        format!("there is no module named '{need}'")
                    };
                    return Err(crate::error::user(
                        format!(
                            "The module '{name}' is selected on host '{}' but needs '{need}', and {reason}.",
                            self.host.id
                        ),
                        &[
                            "Give the dependent module a `when` that is at least as narrow as the module it needs.",
                            "Or widen the needed module's `when` so that it is selected on the same hosts.",
                        ],
                    ));
                }
            }
            graph.add(name, &module.config.needs);
        }
        let order = graph.topo_order().map_err(|cycle| {
            crate::error::user(
                format!("The module `needs` form a cycle: {}.", cycle.join(" -> ")),
                &["Remove one of the `needs` entries so that modules form a tree or DAG."],
            )
        })?;
        Ok(Selection {
            order: order
                .iter()
                .filter_map(|n| selected.get(n.as_str()).copied())
                .collect(),
            skipped,
            indeterminate,
            broken,
            conditional,
        })
    }

    /// Renders and parses every task of a module, computes resource keys,
    /// applies task-level `when` and the run's `select`, and rejects duplicate keys.
    pub fn prepare(
        &self,
        module: &LoadedModule,
        ctx: Arc<HostContext>,
        contexts: &Contexts,
        honour_select: bool,
        state: &State,
    ) -> crate::Result<PreparedModule> {
        let name = module.config.name.as_str();
        let mut tasks: Vec<PreparedTask> = Vec::new();
        let mut warnings = Vec::new();
        let mut seen: HashMap<ResourceKey, usize> = HashMap::new();
        for (index, decl) in module.config.tasks.iter().enumerate() {
            let label = task_label(name, index, decl);
            let task = super::validate::kind_of(decl).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("{label} uses an unknown task kind."),
                    &["Run `buckle catalog` to list the task kinds this Buckle knows about."],
                )
            })?;
            if let Some(problem) = super::validate::command_run_without_id(decl) {
                warnings.push(format!("{label} {problem}"));
            }
            // `when` is decided before the task's fields are rendered, so a
            // task excluded by `when` never needs its templates to render. A
            // `when` that reads `task.id` of a task without an explicit `id`
            // is decided after rendering, once the id (its key) is known.
            let provisional = decl.id.clone().unwrap_or_default();
            let render_ctx = ctx.in_task(&decl.kind, &provisional, &decl.tags);
            let decide = |task_ctx: &HostContext| -> Option<Result<bool, String>> {
                let when = decl.when.as_ref()?;
                Some(
                    contexts
                        .uncertainty(when)
                        .map(Err)
                        .unwrap_or_else(|| task_ctx.try_matches(when)),
                )
            };
            let late_when = decl.id.is_none()
                && decl
                    .when
                    .as_ref()
                    .is_some_and(|w| w.raw().to_ascii_lowercase().contains("task.id"));
            let early = if late_when { None } else { decide(&render_ctx) };
            if matches!(early, Some(Ok(false))) {
                continue;
            }
            let json = self.render_spec(task, &decl.spec, &render_ctx).map_err(|e| {
                crate::error::wrap_user(e, format!("{label} has a field that could not be rendered."), &[
                    "Check the template expressions in the task's fields against the host's facts and vars.",
                    "Secrets cannot be used in task fields; reference them from a file template instead.",
                ])
            })?;
            let spec = task.parse(json).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("{label} is invalid."),
                    &["Compare the task's fields against `buckle catalog`."],
                )
            })?;
            let key = crate::task::resource_key(name, task, &spec, decl.id.as_deref());
            let task_ctx = ctx.in_task(&decl.kind, &key.key, &decl.tags);
            // A `when` that cannot be decided keeps the resource (it stays in
            // the desired set) without planning it.
            let mut undecided = false;
            match if late_when { decide(&task_ctx) } else { early } {
                None | Some(Ok(true)) => {}
                Some(Ok(false)) => continue,
                Some(Err(reason)) => {
                    warnings.push(format!(
                        "{key} is kept but not planned: its `when` cannot be decided ({reason})"
                    ));
                    undecided = true;
                }
            }
            if let Some(previous) = seen.insert(key.clone(), index) {
                let other = &module.config.tasks[previous];
                return Err(crate::error::user(
                    format!(
                        "Two tasks in module '{name}' resolve to the same resource {key}: {} and {}.",
                        task_label(name, previous, other),
                        label
                    ),
                    &[
                        "Give one of the tasks an explicit `id` so that Buckle can tell the resources apart.",
                        "Or merge the two tasks into one.",
                    ],
                ));
            }
            let selected =
                !undecided && (!honour_select || self.select.is_none_or(|f| task_ctx.matches(f)));
            let mut cx = self.cx(name, module.source.clone(), task_ctx, Mode::Plan);
            cx.previous = state.get(&key).cloned().map(Arc::new);
            tasks.push(PreparedTask {
                decl: decl.clone(),
                task,
                spec,
                key,
                cx,
                selected,
            });
        }
        Ok(PreparedModule {
            name: name.to_owned(),
            needs: module.config.needs.clone(),
            ctx,
            source: module.source.clone(),
            tasks,
            provides: module.config.facts.provides.clone(),
            warnings,
            removal: module.config.removal.policy,
        })
    }

    /// Runs every selected task's `plan`, then collects the handlers notified by
    /// the changing steps.
    pub async fn plan_module(
        &self,
        prepared: PreparedModule,
        contexts: &Contexts,
    ) -> crate::Result<RichModulePlan> {
        let platform = self.system.platform();
        let module = prepared.name.clone();
        let mut steps = Vec::new();
        let mut rich = Vec::new();
        let unselected: Vec<ResourceKey> = prepared
            .tasks
            .iter()
            .filter(|t| !t.selected)
            .map(|t| t.key.clone())
            .collect();
        for pt in prepared.tasks.into_iter().filter(|t| t.selected) {
            let span = tracing::info_span!(
                "task.plan",
                buckle.module = %module,
                buckle.task.kind = %pt.decl.kind,
                buckle.task.key = %pt.key.key,
                buckle.changed = tracing::field::Empty,
            );
            let platform_blocked = !pt.task.platforms().contains(platform);
            let planned = async {
                if platform_blocked {
                    return Ok::<_, crate::Error>(Planned::blocked(format!(
                        "the {} task kind does not support {platform}",
                        pt.decl.kind
                    )));
                }
                pt.task.plan(&pt.cx, &pt.spec).await.map_err(|e| {
                    crate::error::wrap_user(
                        e,
                        format!("Planning {} failed.", pt.key),
                        &["Read the inner error; it names what the task could not inspect."],
                    )
                })
            }
            .instrument(span.clone())
            .await?;
            span.record("buckle.changed", planned.is_change());
            steps.push(Step {
                task: TaskRef {
                    module: module.clone(),
                    id: pt.key.key.clone(),
                    kind: pt.decl.kind.clone(),
                    name: pt.decl.name.clone(),
                },
                resource: pt.key.clone(),
                planned,
                locks: pt.task.locks().iter().map(|l| l.to_string()).collect(),
                notifies: pt.decl.notify.clone(),
                always_runs: pt.task.always_runs(&pt.spec),
                removal: removal_policy(pt.decl.removal, prepared.removal, pt.task),
            });
            rich.push(RichStep {
                task: pt.task,
                spec: pt.spec,
                cx: pt.cx,
                retry: pt.decl.retry.clone(),
                platform_blocked,
            });
        }
        let (handler_runs, handlers) = self
            .plan_handlers(&module, &prepared.ctx, &steps, contexts)
            .await?;
        Ok(RichModulePlan {
            plan: ModulePlan {
                module,
                needs: prepared.needs,
                steps,
                handlers: handler_runs,
                provides_facts: prepared.provides,
            },
            steps: rich,
            handlers,
            ctx: prepared.ctx,
            source: prepared.source,
            unselected,
        })
    }

    /// Handlers notified by changing steps: deduplicated by (module, handler),
    /// ordered `systemd.daemon_reload` → `systemd.restart` → `systemd.reload` →
    /// everything else in notification order; a restart of a unit subsumes a
    /// reload of the same unit.
    pub async fn plan_handlers(
        &self,
        module: &str,
        module_ctx: &Arc<HostContext>,
        steps: &[Step],
        contexts: &Contexts,
    ) -> crate::Result<(Vec<HandlerRun>, Vec<RichHandler>)> {
        let mut notified: Vec<(String, String)> = Vec::new();
        for step in steps.iter().filter(|s| s.planned.is_change()) {
            for notify in &step.notifies {
                let target = match notify.split_once(':') {
                    Some((m, h)) => (m.to_owned(), h.to_owned()),
                    None => (module.to_owned(), notify.clone()),
                };
                if !notified.contains(&target) {
                    notified.push(target);
                }
            }
        }
        if notified.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let platform = self.system.platform();
        struct Entry {
            run: HandlerRun,
            rich: RichHandler,
            unit: Option<String>,
            index: usize,
        }
        let mut entries: Vec<Entry> = Vec::new();
        for (index, (owner_name, handler)) in notified.into_iter().enumerate() {
            let owner = self.revision.module(&owner_name).ok_or_else(|| {
                crate::error::user(
                    format!("Module '{module}' notifies '{owner_name}:{handler}', but there is no module '{owner_name}'."),
                    &["Check the `notify` entries in module.yaml; use `module:handler` for another module's handler."],
                )
            })?;
            let decl = owner.config.handlers.get(&handler).ok_or_else(|| {
                crate::error::user(
                    format!("Module '{module}' notifies '{owner_name}:{handler}', but module '{owner_name}' declares no handler '{handler}'."),
                    &["Declare the handler under `handlers:` in the owning module."],
                )
            })?;
            let owner_ctx = if owner_name == module {
                module_ctx.clone()
            } else {
                self.module_context(contexts, &owner.config)?
            };
            let label = format!("The handler '{handler}' of module '{owner_name}'");
            let task = super::validate::kind_of(decl).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("{label} uses an unknown task kind."),
                    &["Run `buckle catalog` to list the task kinds this Buckle knows about."],
                )
            })?;
            let task_ctx = owner_ctx.in_task(&decl.kind, &handler, &decl.tags);
            let json = self.render_spec(task, &decl.spec, &task_ctx).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("{label} has a field that could not be rendered."),
                    &["Check the template expressions in the handler's fields."],
                )
            })?;
            let spec = task.parse(json).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("{label} is invalid."),
                    &["Compare the handler's fields against `buckle catalog`."],
                )
            })?;
            if let Some(when) = &decl.when
                && !task_ctx.matches(when)
            {
                continue;
            }
            let unit = ["name", "unit"].iter().find_map(|f| {
                spec.json
                    .get(*f)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
            let cx = self.cx(&owner_name, owner.source.clone(), task_ctx, Mode::Plan);
            let planned = if !task.platforms().contains(platform) {
                Planned::blocked(format!(
                    "the {} task kind does not support {platform}",
                    decl.kind
                ))
            } else {
                task.plan(&cx, &spec)
                    .instrument(tracing::info_span!("task.plan", buckle.module = %owner_name, buckle.task.kind = %decl.kind, buckle.task.key = %handler))
                    .await
                    .map_err(|e| crate::error::wrap_user(e, format!("Planning {label} failed."), &["Read the inner error; it names what the handler could not inspect."]))?
            };
            let summary = match &planned {
                Planned::Change { diff } => diff.summary().to_owned(),
                Planned::NoOp => format!("{} (no change)", task.describe(&spec)),
                Planned::Blocked { reason } => format!("blocked: {reason}"),
            };
            entries.push(Entry {
                run: HandlerRun {
                    module: owner_name.clone(),
                    handler: handler.clone(),
                    kind: decl.kind.clone(),
                    summary,
                },
                rich: RichHandler {
                    task,
                    spec,
                    cx,
                    planned,
                    retry: decl.retry.clone(),
                },
                unit,
                index,
            });
        }
        let restarted: BTreeSet<String> = entries
            .iter()
            .filter(|e| e.run.kind == "systemd.restart")
            .filter_map(|e| e.unit.clone())
            .collect();
        entries.retain(|e| {
            !(e.run.kind == "systemd.reload"
                && e.unit.as_ref().is_some_and(|u| restarted.contains(u)))
        });
        entries.sort_by_key(|e| (handler_priority(&e.run.kind), e.index));
        Ok(entries.into_iter().map(|e| (e.run, e.rich)).unzip())
    }

    /// Records that left the desired set but were taken over by a desired
    /// resource, so must not be removed: the same kind and key in another
    /// module (a moved task: the record is re-keyed and kept), or a desired
    /// resource that owns one of the record's artefacts (a re-keyed task, e.g.
    /// `file.copy` → `file.template` of the same path: the old record is
    /// forgotten without cleanup). A moved task plans against its old record.
    pub fn rehomes(
        &self,
        state: &State,
        prepared: &mut [PreparedModule],
        exclude_modules: &BTreeSet<String>,
    ) -> Vec<Rehome> {
        let desired: Vec<ResourceKey> = prepared
            .iter()
            .flat_map(|p| p.desired_keys().cloned())
            .collect();
        let (out, moved) = self.find_rehomes(state, &desired, exclude_modules);
        for (to, record) in moved {
            let record = Arc::new(record);
            for task in prepared.iter_mut().flat_map(|p| p.tasks.iter_mut()) {
                if task.key == to {
                    task.cx.previous = Some(record.clone());
                }
            }
        }
        out
    }

    /// [`Planner::rehomes`] over a desired set: the re-homes, and for moved
    /// records the record re-keyed to its new owner.
    pub fn find_rehomes(
        &self,
        state: &State,
        desired: &[ResourceKey],
        exclude_modules: &BTreeSet<String>,
    ) -> (Vec<Rehome>, Vec<(ResourceKey, ResourceRecord)>) {
        let desired_set: BTreeSet<&ResourceKey> = desired.iter().collect();
        let mut by_kind_key: HashMap<(&str, &str), &ResourceKey> = HashMap::new();
        let mut by_identity: HashMap<String, &ResourceKey> = HashMap::new();
        let mut taken: BTreeSet<&ResourceKey> = BTreeSet::new();
        for key in desired {
            by_kind_key
                .entry((key.kind.as_str(), key.key.as_str()))
                .or_insert(key);
            let mut ids: Vec<String> = crate::task::key_identity(&key.key).into_iter().collect();
            if let Some(record) = state.get(key) {
                taken.insert(key);
                ids.extend(record.identities());
            }
            for id in ids {
                by_identity.entry(id).or_insert(key);
            }
        }
        let mut out = Vec::new();
        let mut moved: Vec<(ResourceKey, ResourceRecord)> = Vec::new();
        for record in state.resources.values() {
            if exclude_modules.contains(&record.key.module) || desired_set.contains(&record.key) {
                continue;
            }
            if let Some(to) = by_kind_key
                .get(&(record.key.kind.as_str(), record.key.key.as_str()))
                .copied()
                && taken.insert(to)
            {
                out.push(Rehome {
                    from: record.key.clone(),
                    to: to.clone(),
                    kind: RehomeKind::Moved,
                    summary: format!(
                        "moved to module '{}': the record (introduced in {}) is kept, nothing is removed",
                        to.module, record.introduced_in
                    ),
                });
                let mut rekeyed = record.clone();
                rekeyed.key = to.clone();
                moved.push((to.clone(), rekeyed));
                continue;
            }
            if let Some((id, to)) = record
                .identities()
                .into_iter()
                .find_map(|id| by_identity.get(&id).map(|to| (id, *to)))
            {
                out.push(Rehome {
                    from: record.key.clone(),
                    to: to.clone(),
                    kind: RehomeKind::Superseded,
                    summary: format!(
                        "superseded by {to}, which manages the same {}: forgotten without cleanup",
                        id.replacen(':', " ", 1)
                    ),
                });
            }
        }
        (out, moved)
    }

    /// Orphans of one module (for a deferred module decided at apply time):
    /// its records outside `desired` that no desired resource took over.
    pub fn module_orphans(
        &self,
        state: &State,
        module: &str,
        desired: &[ResourceKey],
    ) -> Vec<(Removal, RichRemoval)> {
        let others: BTreeSet<String> = state
            .resources
            .values()
            .map(|r| r.key.module.clone())
            .filter(|m| m != module)
            .collect();
        let (rehomes, _) = self.find_rehomes(state, desired, &others);
        let skip: BTreeSet<ResourceKey> = rehomes.into_iter().map(|r| r.from).collect();
        self.removals(state, desired, &others, &skip)
    }

    /// Plans a deferred module once the facts it waited for are refreshed:
    /// decides its `when` again, then plans it (with its orphans) or reports it
    /// unselected (all its resources are orphans) or indeterminate.
    pub async fn plan_deferred(
        &self,
        name: &str,
        facts: &Facts,
        state: &State,
        desired: &[ResourceKey],
    ) -> crate::Result<(DeferredPlan, Vec<(Removal, RichRemoval)>)> {
        let module = self.revision.module(name).ok_or_else(|| {
            crate::error::user(
                format!("The revision {} has no module '{name}'.", self.revision.id),
                &["The module was removed from the configuration; nothing can be planned for it."],
            )
        })?;
        let contexts = self.contexts(facts);
        let ctx = self.module_context(&contexts, &module.config)?;
        match contexts.decide(module.config.when.as_ref(), &ctx) {
            Decision::Yes | Decision::Broken(_) => {
                let prepared = self.prepare(module, ctx, &contexts, true, state)?;
                let mut all: Vec<ResourceKey> = desired.to_vec();
                all.extend(prepared.desired_keys().cloned());
                let plan = self.plan_module(prepared, &contexts).await?;
                Ok((
                    DeferredPlan::Planned(plan),
                    self.module_orphans(state, name, &all),
                ))
            }
            Decision::No => Ok((
                DeferredPlan::Unselected,
                self.module_orphans(state, name, desired),
            )),
            Decision::Unknown(reason) => Ok((DeferredPlan::Indeterminate(reason), Vec::new())),
        }
    }

    /// Orphans: resources in state that no selected module wants any more,
    /// skipping modules whose plan is deferred (their keys are unknown yet) or
    /// indeterminate, and records `skip` names (re-homed ones). Ordered for
    /// execution: dependents before their dependencies.
    pub fn removals(
        &self,
        state: &State,
        desired: &[ResourceKey],
        exclude_modules: &BTreeSet<String>,
        skip: &BTreeSet<ResourceKey>,
    ) -> Vec<(Removal, RichRemoval)> {
        let mut graph = Graph::new();
        for m in &self.revision.modules {
            graph.add(&m.config.name, &m.config.needs);
        }
        let topo = graph.topo_order().unwrap_or_default();
        let rank = |module: &str| -> (usize, String) {
            match topo.iter().position(|n| n == module) {
                // Modules the revision no longer has go first (they are the most "removed").
                None => (0, module.to_owned()),
                Some(i) => (topo.len() - i, String::new()),
            }
        };
        let mut orphans: Vec<&ResourceRecord> = state
            .resources
            .values()
            .filter(|r| {
                !exclude_modules.contains(&r.key.module)
                    && !desired.contains(&r.key)
                    && !skip.contains(&r.key)
            })
            .collect();
        orphans.sort_by(|a, b| {
            rank(&a.key.module)
                .cmp(&rank(&b.key.module))
                .then_with(|| b.applied_at.cmp(&a.applied_at))
                .then_with(|| b.key.cmp(&a.key))
        });
        orphans
            .into_iter()
            .map(|record| {
                let task = crate::task::registry().get(record.key.kind.as_str()).copied();
                let summary = match (record.removal, task) {
                    (RemovalPolicy::Leave, _) => "released: left in place (policy leave)".to_owned(),
                    (RemovalPolicy::Cleanup, Some(t)) => t.describe_removal(record),
                    (RemovalPolicy::Cleanup, None) => format!(
                        "the task kind '{}' is no longer available; the resource will be forgotten but not cleaned up",
                        record.key.kind
                    ),
                };
                (
                    Removal {
                        resource: record.key.clone(),
                        policy: record.removal,
                        summary,
                        caution: record.removal == RemovalPolicy::Cleanup
                            && task.is_some_and(|t| t.removal_caution()),
                        introduced_in: Some(record.introduced_in.clone()),
                    },
                    RichRemoval {
                        record: record.clone(),
                        task,
                    },
                )
            })
            .collect()
    }
}

/// A task's removal policy: its own `removal`, else the module's
/// `removal.policy`, else the kind's default.
fn removal_policy(
    task: Option<RemovalPolicy>,
    module: Option<RemovalPolicy>,
    kind: &'static dyn DynTask,
) -> RemovalPolicy {
    task.or(module).unwrap_or_else(|| kind.removal_default())
}

fn handler_priority(kind: &str) -> u8 {
    match kind {
        "systemd.daemon_reload" => 0,
        "systemd.restart" => 1,
        "systemd.reload" => 2,
        _ => 3,
    }
}

fn task_label(module: &str, index: usize, decl: &TaskDecl) -> String {
    match &decl.name {
        Some(n) => format!(
            "task #{} '{n}' ({}) of module '{module}'",
            index + 1,
            decl.kind
        ),
        None => format!("task #{} ({}) of module '{module}'", index + 1, decl.kind),
    }
}

/// Collects facts unless the inputs carry a frozen snapshot.
pub(super) async fn collect_facts(inputs: &Inputs, planner: &Planner<'_>) -> Facts {
    let mut facts = inputs.facts.clone();
    if facts.tree.is_null() {
        facts.tree = serde_json::Value::Object(Default::default());
    }
    if !inputs.facts_frozen {
        let cx = planner.facts_cx();
        crate::facts::collect(&cx, &mut facts, None, false).await;
    }
    facts
}

/// Plans the whole revision for the host described by `inputs`.
pub(super) async fn plan(inputs: &Inputs, revision: &Revision) -> crate::Result<(RichPlan, Facts)> {
    plan_mode(inputs, revision, inputs.drift).await
}

/// [`plan`], in drift mode or not.
pub(super) async fn plan_mode(
    inputs: &Inputs,
    revision: &Revision,
    drift: bool,
) -> crate::Result<(RichPlan, Facts)> {
    let mut planner = Planner::from_inputs(inputs, revision)?;
    planner.drift = drift;
    let facts = collect_facts(inputs, &planner).await;
    let state = inputs.state.load()?;
    let rich = plan_with(&planner, &facts, &state).await?;
    Ok((rich, facts))
}

/// Plans against an already collected facts snapshot and state.
pub(super) async fn plan_with(
    planner: &Planner<'_>,
    facts: &Facts,
    state: &State,
) -> crate::Result<RichPlan> {
    let contexts = planner.contexts(facts);
    let mut selection = planner.select(&contexts)?;
    let revision = planner.revision;

    // Drift re-plans only what this host has applied at this revision.
    let mut held: Vec<Held> = Vec::new();
    if planner.drift {
        let id = &revision.id;
        selection.order.retain(|m| {
            let name = &m.config.name;
            if state.module_at(name, id) {
                return true;
            }
            let reason = match (
                state.failed_modules.get(name),
                state.module_revisions.get(name),
            ) {
                (Some(failed), _) => {
                    format!("its last run failed (revision {failed}); drift does not re-apply it")
                }
                (None, Some(other)) => format!("it is applied at revision {other}, not {id}"),
                (None, None) => format!("it has not been applied at revision {id}"),
            };
            held.push(Held {
                module: name.clone(),
                reason,
            });
            false
        });
        let (keep, unapplied): (Vec<_>, Vec<_>) = std::mem::take(&mut selection.conditional)
            .into_iter()
            .partition(|(m, _)| state.module_at(&m.config.name, id));
        selection.conditional = keep;
        selection
            .skipped
            .extend(unapplied.into_iter().map(|(m, _)| m.config.name.clone()));
    }

    // Pass 1: render and parse every selected module. A failure is scoped to
    // its module (decided in pass 2: deferred or a plan error).
    let mut failures: HashMap<String, String> = selection.broken.clone();
    let mut prepared: Vec<PreparedModule> = Vec::with_capacity(selection.order.len());
    for module in &selection.order {
        if failures.contains_key(&module.config.name) {
            continue;
        }
        let attempt = planner
            .module_context(&contexts, &module.config)
            .and_then(|ctx| planner.prepare(module, ctx, &contexts, true, state));
        match attempt {
            Ok(p) => prepared.push(p),
            Err(err) => {
                failures.insert(module.config.name.clone(), planner.failure(&err));
            }
        }
    }
    let prepared_warnings: Vec<String> = prepared
        .iter()
        .flat_map(|p| p.warnings.iter().cloned())
        .collect();
    let provides: HashMap<String, Vec<String>> = selection
        .order
        .iter()
        .map(|m| (m.config.name.clone(), m.config.facts.provides.clone()))
        .collect();

    let mut undecided: BTreeSet<String> = selection
        .indeterminate
        .iter()
        .map(|i| i.module.clone())
        .chain(held.iter().map(|h| h.module.clone()))
        .chain(failures.keys().cloned())
        .chain(
            selection
                .conditional
                .iter()
                .map(|(m, _)| m.config.name.clone()),
        )
        .collect();
    let rehomed = planner.rehomes(state, &mut prepared, &undecided);
    let mut rehomed_from: BTreeSet<ResourceKey> = rehomed.iter().map(|r| r.from.clone()).collect();

    // Pass 2: plan in dependency order, deferring modules that wait for facts
    // a provider module produces in this run.
    let mut by_name: HashMap<String, PreparedModule> =
        prepared.into_iter().map(|p| (p.name.clone(), p)).collect();
    let mut planned: Vec<RichModulePlan> = Vec::new();
    let mut deferred: Vec<Deferred> = Vec::new();
    let mut problems: Vec<Indeterminate> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut needs: HashMap<String, Vec<String>> = HashMap::new();
    let pending = |name: &str, planned: &[RichModulePlan], deferred: &[Deferred]| {
        provides.get(name).is_some_and(|p| !p.is_empty())
            && (deferred.iter().any(|d| d.module == name)
                || planned
                    .iter()
                    .any(|m| m.plan.module == name && m.plan.changes() > 0))
    };
    for module in &selection.order {
        let name = module.config.name.clone();
        if let Some(dep) = module
            .config
            .needs
            .iter()
            .find(|d| problems.iter().any(|p| &p.module == *d))
        {
            problems.push(Indeterminate {
                module: name,
                reason: format!("it needs '{dep}', which could not be planned"),
                plan_error: false,
            });
            continue;
        }
        let mut waiting_on = Vec::new();
        let mut reasons = Vec::new();
        for dep in &module.config.needs {
            if deferred.iter().any(|d| &d.module == dep) {
                waiting_on.push(dep.clone());
                reasons.push(format!("waits for {dep}, which is deferred"));
            } else if pending(dep, &planned, &deferred) {
                waiting_on.push(dep.clone());
                reasons.push(format!("waits for facts from {dep}"));
            }
        }
        if let Some(reason) = failures.get(&name) {
            // A render failure that reads facts a provider it needs has yet to
            // produce is expected on a first install: wait for the provider.
            let refs = context::module_fact_references(revision, &module.config);
            let providers: Vec<String> = ancestors(revision, &name)
                .into_iter()
                .filter(|p| {
                    pending(p, &planned, &deferred)
                        && refs
                            .iter()
                            .any(|r| context::provided_by(r, &provides[p.as_str()]))
                })
                .collect();
            if providers.is_empty() && reasons.is_empty() {
                problems.push(Indeterminate {
                    module: name,
                    reason: reason.clone(),
                    plan_error: true,
                });
                continue;
            }
            for p in providers {
                if !waiting_on.contains(&p) {
                    reasons.push(format!("reads facts that {p} provides"));
                    waiting_on.push(p);
                }
            }
        }
        if !reasons.is_empty() {
            let mut waits = module.config.needs.clone();
            waits.extend(
                waiting_on
                    .iter()
                    .filter(|w| !module.config.needs.contains(w))
                    .cloned(),
            );
            needs.insert(name.clone(), waits);
            order.push(name.clone());
            deferred.push(Deferred {
                module: name,
                waiting_on,
                reason: reasons.join("; "),
            });
            continue;
        }
        let Some(prepared) = by_name.remove(&name) else {
            continue;
        };
        match planner.plan_module(prepared, &contexts).await {
            Ok(m) => {
                needs.insert(name.clone(), module.config.needs.clone());
                order.push(name);
                planned.push(m);
            }
            Err(err) => problems.push(Indeterminate {
                module: name,
                reason: format!("planning failed: {}", planner.failure(&err)),
                plan_error: true,
            }),
        }
    }
    // Modules whose `when` reads facts a changing provider produces are
    // decided after that provider applies; otherwise they are skipped.
    let mut skipped = selection.skipped;
    for (module, providers) in &selection.conditional {
        let name = module.config.name.clone();
        let waiting_on: Vec<String> = providers
            .iter()
            .filter(|p| pending(p, &planned, &deferred))
            .cloned()
            .collect();
        if waiting_on.is_empty() {
            undecided.remove(&name);
            skipped.push(name);
            continue;
        }
        let mut waits = module.config.needs.clone();
        waits.extend(
            waiting_on
                .iter()
                .filter(|w| !module.config.needs.contains(w))
                .cloned(),
        );
        needs.insert(name.clone(), waits);
        order.push(name.clone());
        deferred.push(Deferred {
            module: name,
            reason: format!(
                "its `when` reads facts from {}; decided once {} applied",
                waiting_on.join(", "),
                if waiting_on.len() == 1 {
                    "it has"
                } else {
                    "they have"
                }
            ),
            waiting_on,
        });
    }
    // Planned modules' desired keys (deferred modules' resources are excluded
    // from orphans below and decided at apply time).
    let desired: Vec<ResourceKey> = planned
        .iter()
        .flat_map(|m| m.plan.steps.iter().map(|s| s.resource.clone()))
        .chain(by_name.values().flat_map(|p| p.desired_keys().cloned()))
        .collect();
    let desired: Vec<ResourceKey> = {
        // Unselected-by-`--select` tasks stay desired too.
        let mut all = desired;
        for m in &planned {
            all.extend(m.unselected.iter().cloned());
        }
        all
    };

    let exclude: BTreeSet<String> = deferred
        .iter()
        .map(|d| d.module.clone())
        .chain(undecided.iter().cloned())
        .chain(problems.iter().map(|p| p.module.clone()))
        .collect();
    // Drift never removes what a revision other than its own recorded.
    if planner.drift {
        rehomed_from.extend(
            state
                .resources
                .values()
                .filter(|r| r.revision != revision.id)
                .map(|r| r.key.clone()),
        );
    }
    let (removals, rich_removals): (Vec<Removal>, Vec<RichRemoval>) = planner
        .removals(state, &desired, &exclude, &rehomed_from)
        .into_iter()
        .unzip();

    let module_plans: Vec<ModulePlan> = planned.iter().map(|m| m.plan.clone()).collect();
    let mut plan = Plan::new(
        revision.id.clone(),
        planner.host.id.clone(),
        module_plans,
        removals,
        deferred,
    );
    plan.skipped = skipped;
    let platform_blocked: BTreeSet<ResourceKey> = planned
        .iter()
        .flat_map(|m| m.plan.steps.iter().zip(m.steps.iter()))
        .filter(|(_, rs)| rs.platform_blocked)
        .map(|(s, _)| s.resource.clone())
        .collect();
    let mut indeterminate = selection.indeterminate;
    indeterminate.extend(problems);
    indeterminate.sort_by(|a, b| a.module.cmp(&b.module));
    let mut plan = plan
        .with_indeterminate(indeterminate)
        .with_rehomed(rehomed)
        .with_held(held);
    plan.warnings = warnings(&plan, &platform_blocked);
    plan.warnings.extend(prepared_warnings);
    Ok(RichPlan {
        plan,
        modules: planned,
        removals: rich_removals,
        order,
        needs,
        desired,
    })
}

/// Every module `name` needs, directly or transitively.
fn ancestors(revision: &Revision, name: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![name.to_owned()];
    while let Some(current) = stack.pop() {
        if let Some(m) = revision.module(&current) {
            for need in &m.config.needs {
                if out.insert(need.clone()) {
                    stack.push(need.clone());
                }
            }
        }
    }
    out
}

/// Operator-facing warnings: indeterminate modules, removals, always-run
/// steps, blocked steps.
pub(super) fn warnings(plan: &Plan, platform_blocked: &BTreeSet<ResourceKey>) -> Vec<String> {
    let mut out = Vec::new();
    for i in &plan.indeterminate {
        if i.plan_error {
            out.push(format!(
                "module '{}' could not be planned: {}; it will not be applied and its resources are kept",
                i.module, i.reason
            ));
        } else {
            out.push(format!(
                "module '{}' is indeterminate: {}; it will not be applied and its resources are kept",
                i.module, i.reason
            ));
        }
    }
    for r in &plan.removals {
        let caution = if r.caution {
            " (caution: removing this can take unrelated things down)"
        } else {
            ""
        };
        out.push(format!("removal: {} — {}{caution}", r.resource, r.summary));
    }
    for r in &plan.rehomed {
        out.push(format!("not removed: {} — {}", r.from, r.summary));
    }
    for h in &plan.held {
        out.push(format!("module '{}' is held: {}", h.module, h.reason));
    }
    for step in plan.steps() {
        if step.always_runs {
            out.push(format!(
                "{} always runs: it has no idempotency guard (creates, unless or changed_when)",
                step.resource
            ));
        }
        if let Planned::Blocked { reason } = &step.planned {
            if platform_blocked.contains(&step.resource) {
                out.push(format!("{} is blocked: {reason}", step.resource));
            } else {
                out.push(format!(
                    "{} is blocked: {reason} (planned again at apply time once earlier steps have run)",
                    step.resource
                ));
            }
        }
    }
    out
}

/// Everything up to (not including) task `plan` for a host, with placeholder
/// secrets, for `buckle validate --facts`.
pub(super) async fn dry_run(
    revision: &Revision,
    host: &HostIdentity,
    facts: &Facts,
) -> crate::Result<()> {
    let mut secrets = SecretStore::new();
    let key = crate::secrets::InstallKey::from_bytes([0u8; 32]);
    for name in revision.fleet.secrets.keys() {
        secrets.insert(crate::secrets::Secret::new(
            name.clone(),
            "placeholder".into(),
            &key,
        ));
    }
    let agent_vars = Vars::new();
    let planner = Planner::new(
        revision,
        host.clone(),
        Arc::new(crate::system::RealSystem::new()),
        Arc::new(secrets),
        None,
        Arc::new(Vec::new()),
        &agent_vars,
    )?;
    let contexts = planner.contexts(facts);
    let selection = planner.select(&contexts)?;
    for module in &selection.order {
        let ctx = planner.module_context(&contexts, &module.config)?;
        planner.prepare(module, ctx.clone(), &contexts, false, &State::default())?;
        for (name, decl) in &module.config.handlers {
            let task = super::validate::kind_of(decl)?;
            let task_ctx = ctx.in_task(&decl.kind, name, &decl.tags);
            let json = planner.render_spec(task, &decl.spec, &task_ctx).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("The handler '{name}' of module '{}' has a field that could not be rendered.", module.config.name),
                    &["Check the template expressions in the handler's fields."],
                )
            })?;
            task.parse(json)?;
        }
        for (index, decl) in module.config.health.iter().enumerate() {
            let check = crate::health::lookup(&decl.check.kind)?;
            let json = context::render_value(&decl.check.spec, &planner.templater, &ctx).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("Health check #{} of module '{}' has a field that could not be rendered.", index + 1, module.config.name),
                    &["Check the template expressions in the check's fields."],
                )
            })?;
            check.parse(json)?;
        }
    }
    Ok(())
}
