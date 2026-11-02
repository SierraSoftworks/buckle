//! The pure half of the engine: facts, groups, vars, module selection and
//! ordering, task preparation and planning, handler collection, deferral and
//! removals. Nothing here writes to the host.

use super::context;
use super::graph::Graph;
use super::{Inputs, LoadedModule, Revision};
use crate::facts::Facts;
use crate::model::{ModuleConfig, RemovalPolicy, RetryPolicy, TaskDecl};
use crate::plan::{
    Deferred, HandlerRun, Indeterminate, ModulePlan, Plan, Planned, Removal, ResourceKey, Step,
    TaskRef,
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
    pub needs: HashMap<String, Vec<String>>,
    pub provides: HashMap<String, Vec<String>>,
}

/// Module selection for a host.
pub(super) struct Selection<'a> {
    /// Selected modules in dependency order.
    pub order: Vec<&'a LoadedModule>,
    pub skipped: Vec<String>,
    /// Modules whose selection could not be decided or that could not be
    /// planned, sorted by name.
    pub indeterminate: Vec<Indeterminate>,
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
            schemas: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_inputs(inputs: &'a Inputs, revision: &'a Revision) -> crate::Result<Self> {
        Self::new(
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
        )
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

    /// Evaluates every module's `when`, checks that selected modules only need
    /// selected modules, and orders them along the DAG. A module whose
    /// selection cannot be decided (its `when` errs or reads unavailable facts,
    /// its vars fail to render, its directory has no module.yaml, or it needs
    /// such a module) is indeterminate, never "unselected".
    pub fn select(&self, contexts: &Contexts) -> crate::Result<Selection<'a>> {
        let mut selected: HashMap<&str, &'a LoadedModule> = HashMap::new();
        let mut skipped = Vec::new();
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
                Decision::No => skipped.push(cfg.name.clone()),
                Decision::Unknown(reason) => indeterminate.push(Indeterminate {
                    module: cfg.name.clone(),
                    reason,
                    plan_error: false,
                }),
                Decision::Broken(reason) => indeterminate.push(Indeterminate {
                    module: cfg.name.clone(),
                    reason,
                    plan_error: true,
                }),
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
            let provisional = decl.id.clone().unwrap_or_default();
            let render_ctx = ctx.in_task(&decl.kind, &provisional, &decl.tags);
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
            if let Some(when) = &decl.when {
                let doubt = contexts
                    .uncertainty(when)
                    .map(Err)
                    .unwrap_or_else(|| task_ctx.try_matches(when));
                match doubt {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(reason) => {
                        warnings.push(format!(
                            "{key} is kept but not planned: its `when` cannot be decided ({reason})"
                        ));
                        undecided = true;
                    }
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

    /// Prepares and plans one module against `facts` (for deferred modules and
    /// rollback). `honour_select` is false for rollback, which re-applies the
    /// whole module.
    pub async fn plan_one(
        &self,
        name: &str,
        facts: &Facts,
        state: &State,
        honour_select: bool,
    ) -> crate::Result<RichModulePlan> {
        let module = self.revision.module(name).ok_or_else(|| {
            crate::error::user(
                format!("The revision {} has no module '{name}'.", self.revision.id),
                &["The module was removed from the configuration; nothing can be planned for it."],
            )
        })?;
        let contexts = self.contexts(facts);
        let ctx = self.module_context(&contexts, &module.config)?;
        let prepared = self.prepare(module, ctx, &contexts, honour_select, state)?;
        self.plan_module(prepared, &contexts).await
    }

    /// Orphans: resources in state that no selected module wants any more,
    /// skipping modules whose plan is deferred (their keys are unknown yet).
    /// Ordered for execution: dependents before their dependencies.
    pub fn removals(
        &self,
        state: &State,
        desired: &[ResourceKey],
        exclude_modules: &BTreeSet<String>,
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
            .filter(|r| !exclude_modules.contains(&r.key.module) && !desired.contains(&r.key))
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
    let planner = Planner::from_inputs(inputs, revision)?;
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
    let selection = planner.select(&contexts)?;

    let mut prepared = Vec::with_capacity(selection.order.len());
    for module in &selection.order {
        let ctx = planner.module_context(&contexts, &module.config)?;
        prepared.push(planner.prepare(module, ctx, &contexts, true, state)?);
    }
    let desired: Vec<ResourceKey> = prepared
        .iter()
        .flat_map(|p| p.desired_keys().cloned())
        .collect();
    let order: Vec<String> = prepared.iter().map(|p| p.name.clone()).collect();
    let prepared_warnings: Vec<String> = prepared
        .iter()
        .flat_map(|p| p.warnings.iter().cloned())
        .collect();
    let needs: HashMap<String, Vec<String>> = prepared
        .iter()
        .map(|p| (p.name.clone(), p.needs.clone()))
        .collect();
    let provides: HashMap<String, Vec<String>> = prepared
        .iter()
        .map(|p| (p.name.clone(), p.provides.clone()))
        .collect();

    let mut planned: Vec<RichModulePlan> = Vec::new();
    let mut deferred: Vec<Deferred> = Vec::new();
    for module in prepared {
        let mut waiting_on = Vec::new();
        let mut reasons = Vec::new();
        for dep in &module.needs {
            if deferred.iter().any(|d| &d.module == dep) {
                waiting_on.push(dep.clone());
                reasons.push(format!("waits for {dep}, which is deferred"));
            } else if provides.get(dep).is_some_and(|p| !p.is_empty())
                && planned
                    .iter()
                    .any(|m| &m.plan.module == dep && m.plan.changes() > 0)
            {
                waiting_on.push(dep.clone());
                reasons.push(format!("waits for facts from {dep}"));
            }
        }
        if !reasons.is_empty() {
            deferred.push(Deferred {
                module: module.name.clone(),
                waiting_on,
                reason: reasons.join("; "),
            });
            continue;
        }
        planned.push(planner.plan_module(module, &contexts).await?);
    }

    let exclude: BTreeSet<String> = deferred
        .iter()
        .map(|d| d.module.clone())
        .chain(selection.indeterminate.iter().map(|i| i.module.clone()))
        .collect();
    let (removals, rich_removals): (Vec<Removal>, Vec<RichRemoval>) = planner
        .removals(state, &desired, &exclude)
        .into_iter()
        .unzip();

    let task_warnings: Vec<String> = prepared_warnings;
    let module_plans: Vec<ModulePlan> = planned.iter().map(|m| m.plan.clone()).collect();
    let mut plan = Plan::new(
        planner.revision.id.clone(),
        planner.host.id.clone(),
        module_plans,
        removals,
        deferred,
    );
    plan.skipped = selection.skipped;
    let platform_blocked: BTreeSet<ResourceKey> = planned
        .iter()
        .flat_map(|m| m.plan.steps.iter().zip(m.steps.iter()))
        .filter(|(_, rs)| rs.platform_blocked)
        .map(|(s, _)| s.resource.clone())
        .collect();
    let mut plan = plan.with_indeterminate(selection.indeterminate);
    plan.warnings = warnings(&plan, &platform_blocked);
    plan.warnings.extend(task_warnings);
    Ok(RichPlan {
        plan,
        modules: planned,
        removals: rich_removals,
        order,
        needs,
        provides,
    })
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
