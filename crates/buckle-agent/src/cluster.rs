//! The agent's second pipeline: cluster targets it executes for (design:
//! cluster workloads, D6 and D7).
//!
//! * The server picks this host as a cluster's executor and sends a targeted
//!   Assign ([`Assignment`]) ahead of every plan, apply, rollback and health
//!   request for it, so nothing about the cluster is kept between them.
//! * [`Executor`] compiles the revision's selected workloads into
//!   [`buckle_nomad`] primitives, plans them into an ordinary
//!   [`Plan`] (one module per workload), applies them in `needs` order and
//!   runs their implicit health checks. Ownership lives in the cluster
//!   (stamps and the index variable); objects of workloads that went away
//!   are planned as removals.
//! * Credentials come from Buckle-issued identities ([`crate::identity`]),
//!   the host's own Nomad token (`local`, `workload`) or a `secret://`
//!   secret.

use crate::identity::{Credential, Credentials, IdentityRequest, Login, Purpose};
use crate::transport::Transport;
use buckle_core::engine::{
    ApplyEvent, ApplyOutcome, LoadedWorkload, ModuleOutcome, ModuleStatus, RemovalOutcome,
    Revision, StepOutcome, StepState,
};
use buckle_core::error::ErrorReport;
use buckle_core::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use buckle_core::model::{ClusterIdentity, ClusterKind, RemovalPolicy, WorkloadRemoval};
use buckle_core::plan::{
    Indeterminate, ModulePlan, Plan, Removal, ResourceKey, RevisionId, Step, TaskRef,
};
use buckle_core::secrets::sinks::ApiSink;
use buckle_core::secrets::{InstallKey, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity};
use buckle_core::template::Templater;
use buckle_nomad::primitives::{
    Attach, EntityKind, EntitySpec, JobSpec, Orphan, PolicySpec, VariableSpec, VolumeSpec,
};
use buckle_nomad::{Context, Kind, Nomad, ObjectKey, Workload, WorkloadPlan};
use chrono::Utc;
use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// The scheduler's local API when a cluster names none and `NOMAD_ADDR` is
/// unset.
pub const DEFAULT_NOMAD_ADDR: &str = "http://127.0.0.1:4646";

/// A cluster target as its latest targeted Assign described it.
#[derive(Clone)]
pub struct TargetSpec {
    pub kind: ClusterKind,
    /// The cluster's API; `None`: the executor's local default.
    pub api: Option<String>,
    pub identity: ClusterIdentity,
    /// Effective labels, for `cluster.labels.*`.
    pub labels: IndexMap<String, String>,
    /// Keys plan fingerprints (the same for every executor of the target).
    pub fingerprints: Arc<InstallKey>,
}

impl std::fmt::Debug for TargetSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetSpec")
            .field("kind", &self.kind)
            .field("api", &self.api)
            .field("identity", &self.identity)
            .field("labels", &self.labels)
            .finish_non_exhaustive()
    }
}

/// One revision of one cluster target, ready to plan, apply or check.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub target: String,
    pub spec: Arc<TargetSpec>,
    pub revision: Arc<Revision>,
    /// The secrets the target's selected workloads (and identity) reference.
    pub secrets: Arc<SecretStore>,
}

impl Assignment {
    /// The cluster's selector context: `cluster.id`, `cluster.kind`,
    /// `cluster.labels.*`, its groups, and the assigned secrets.
    pub fn context(&self) -> HostContext {
        buckle_core::engine::cluster_context(
            &self.revision,
            HostIdentity {
                id: self.target.clone(),
                labels: buckle_core::selectors::typed_labels(&self.spec.labels),
            },
            self.spec.kind,
        )
        .with_secrets(self.secrets.clone())
    }

    /// The selected workloads, in `needs` order (a workload after the
    /// selected workloads it needs; otherwise as the revision lists them).
    pub fn workloads(&self, ctx: &HostContext) -> Vec<&LoadedWorkload> {
        order(buckle_core::engine::selected_workloads(&self.revision, ctx))
    }
}

/// Builds the API client for a cluster target. Tests substitute an
/// in-memory Nomad.
pub trait Connect: Send + Sync + 'static {
    fn nomad(&self, target: &str, spec: &TargetSpec) -> buckle_core::Result<Nomad>;
}

/// Connects over the network (or a Unix socket) to the cluster's `api`.
#[derive(Debug, Default)]
pub struct Network;

impl Connect for Network {
    fn nomad(&self, _target: &str, spec: &TargetSpec) -> buckle_core::Result<Nomad> {
        Nomad::connect(&nomad_address(spec), None)
    }
}

/// The Nomad API a target is reached at: its `api`, else `NOMAD_ADDR`, else
/// the local agent.
pub fn nomad_address(spec: &TargetSpec) -> String {
    spec.api
        .clone()
        .filter(|a| !a.is_empty())
        .or_else(|| std::env::var("NOMAD_ADDR").ok().filter(|a| !a.is_empty()))
        .unwrap_or_else(|| DEFAULT_NOMAD_ADDR.to_owned())
}

/// Plans, applies and checks cluster targets.
pub struct Executor {
    credentials: Credentials,
    connect: Arc<dyn Connect>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor").finish_non_exhaustive()
    }
}

/// One workload's part of a plan.
struct Unit {
    name: String,
    cx: Context,
    plan: WorkloadPlan,
}

/// A cluster plan: what the server sees ([`ClusterPlan::plan`]) and what
/// the apply sends.
pub struct ClusterPlan {
    pub plan: Plan,
    units: Vec<Unit>,
    removals: Vec<(Orphan, buckle_nomad::Removal, Context)>,
}

impl std::fmt::Debug for ClusterPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterPlan")
            .field("plan", &self.plan.digest)
            .finish_non_exhaustive()
    }
}

impl Executor {
    pub fn new(transport: Arc<dyn Transport>, connect: Arc<dyn Connect>) -> Self {
        Self {
            credentials: Credentials::new(transport, reqwest::Client::new()),
            connect,
        }
    }

    /// A client for `a`'s cluster carrying a credential for `purpose`.
    async fn client(
        &self,
        a: &Assignment,
        purpose: Purpose,
        rollout: Option<&str>,
    ) -> buckle_core::Result<Nomad> {
        if a.spec.kind != ClusterKind::Nomad {
            return Err(buckle_core::error::user(
                format!(
                    "{} is a {} cluster; this agent executes Nomad clusters only.",
                    a.target, a.spec.kind
                ),
                &["Kubernetes executors arrive in a later Buckle release."],
            ));
        }
        let nomad = self.connect.nomad(&a.target, &a.spec)?;
        match &a.spec.identity {
            ClusterIdentity::BuckleJwt => {
                let request = IdentityRequest {
                    target: a.target.clone(),
                    purpose,
                    revision: Some(a.revision.id.clone()),
                    rollout: rollout.map(str::to_owned),
                };
                let address = nomad_address(&a.spec);
                let login = Login::Nomad {
                    api: url::Url::parse(&address)
                        .or_else(|_| url::Url::parse(DEFAULT_NOMAD_ADDR))
                        .map_err(|e| {
                            buckle_core::error::wrap_system(e, "The Nomad address is invalid.", &[])
                        })?,
                    auth_method: Login::DEFAULT_NOMAD_AUTH_METHOD.to_owned(),
                };
                let anonymous = nomad.clone();
                let target = a.target.clone();
                let credential = self
                    .credentials
                    .get_with(&request, &login, |identity| async move {
                        nomad_login(&anonymous, &target, &identity).await
                    })
                    .await?;
                let (_, token) = credential.header();
                Ok(nomad.token(token.into()))
            }
            ClusterIdentity::Local | ClusterIdentity::Workload => {
                Ok(match std::env::var("NOMAD_TOKEN") {
                    Ok(token) if !token.is_empty() => nomad.token(token.into()),
                    // The task API socket (an executor inside the cluster)
                    // authenticates on its own.
                    _ => nomad,
                })
            }
            ClusterIdentity::Secret(name) => match a.secrets.get(name) {
                Some(secret) => Ok(nomad.token(ApiSink.credential(secret))),
                None => Err(buckle_core::error::user(
                    format!(
                        "The credential secret '{name}' for {} did not reach this executor.",
                        a.target
                    ),
                    &[
                        "Check that buckle.yaml declares it under `secrets` and the server can resolve it.",
                    ],
                )),
            },
        }
    }

    /// Plans `a`. Pure: reads the cluster, changes nothing. A workload that
    /// cannot be planned is indeterminate (its objects are never orphans);
    /// one that is unreachable fails the plan.
    pub async fn plan(
        &self,
        a: &Assignment,
        purpose: Purpose,
        rollout: Option<&str>,
    ) -> buckle_core::Result<ClusterPlan> {
        let nomad = self.client(a, purpose, rollout).await?;
        let ctx = a.context();
        let workloads = a.workloads(&ctx);
        let implicit = implicit_namespaces(&nomad, &workloads).await?;

        let mut units = Vec::new();
        let mut modules = Vec::new();
        let mut warnings = Vec::new();
        let mut indeterminate = Vec::new();
        let mut declared: Vec<ObjectKey> = implicit.present.clone();
        let mut undecided: BTreeSet<String> = BTreeSet::new();
        let mut earlier: Vec<ObjectKey> = Vec::new();
        let mut namespaces_created: BTreeSet<String> = BTreeSet::new();

        for w in workloads {
            let name = w.config.name.clone();
            let compiled = if w.config.uses_kubernetes_keys() {
                Err(buckle_core::error::user(
                    format!(
                        "The workload {name} uses Kubernetes keys, but {} is a Nomad cluster.",
                        a.target
                    ),
                    &["Narrow the workload's `when` to `cluster.kind == \"kubernetes\"`."],
                ))
            } else {
                compile(&a.revision, w, &ctx).await
            };
            let planned = match compiled {
                Ok(mut workload) => {
                    if let Some(ns) = w.config.namespace.as_deref()
                        && implicit.missing.contains(ns)
                        && namespaces_created.insert(ns.to_owned())
                    {
                        workload.namespaces.push(implicit_namespace(ns));
                    }
                    let cx = Context::new(
                        nomad.clone(),
                        &a.target,
                        &name,
                        &a.revision.id.0,
                        a.spec.fingerprints.clone(),
                    );
                    workload
                        .plan_after(&cx, &earlier)
                        .await
                        .map(|plan| (cx, plan))
                }
                Err(e) => Err(e),
            };
            match planned {
                Ok((cx, plan)) => {
                    earlier.extend(plan.created());
                    declared.extend(plan.keys());
                    for step in &plan.steps {
                        warnings.extend(
                            step.warnings
                                .iter()
                                .map(|m| format!("{name}: {}: {m}", step.key)),
                        );
                        warnings.extend(
                            step.preview
                                .iter()
                                .map(|m| format!("{name}: {}: {m}", step.key)),
                        );
                    }
                    modules.push(module_plan(&name, &w.config.needs, &plan));
                    units.push(Unit { name, cx, plan });
                }
                Err(e) => {
                    tracing::warn!(target = %a.target, workload = %name, error = %e.description(), "a workload could not be planned");
                    indeterminate.push(Indeterminate::plan_error(
                        &name,
                        e.description(),
                        Some(ErrorReport::of(&e)),
                    ));
                    undecided.insert(name);
                }
            }
        }

        let cx = Context::new(
            nomad.clone(),
            &a.target,
            "",
            &a.revision.id.0,
            a.spec.fingerprints.clone(),
        );
        let mut removals = Vec::new();
        let mut planned_removals = Vec::new();
        for orphan in buckle_nomad::primitives::orphans(&cx, &declared).await? {
            // A workload whose plan failed declares nothing we know of.
            if undecided.contains(&orphan.module) {
                continue;
            }
            let removal = a
                .revision
                .workload(&orphan.module)
                .map(|w| nomad_removal(w.config.removal))
                .unwrap_or_default();
            let summary = orphan
                .describe(removal)
                .unwrap_or_else(|| format!("keep {} in the cluster", orphan.key));
            planned_removals.push(Removal {
                resource: resource_key(&orphan.module, &orphan.key),
                policy: if orphan.describe(removal).is_some() {
                    RemovalPolicy::Cleanup
                } else {
                    RemovalPolicy::Leave
                },
                summary,
                caution: orphan.caution(removal),
                introduced_in: None,
            });
            let mut ocx = cx.clone();
            ocx.module = orphan.module.clone();
            removals.push((orphan, removal, ocx));
        }

        let mut plan = Plan::new(
            a.revision.id.clone(),
            &a.target,
            modules,
            planned_removals,
            Vec::new(),
        )
        .with_indeterminate(indeterminate);
        plan.warnings = warnings;
        Ok(ClusterPlan {
            plan,
            units,
            removals,
        })
    }

    /// Applies `planned` in order, stopping at the first failure; then its
    /// removals. Events stream to `events`; the outcome says what happened.
    pub async fn apply(
        &self,
        planned: &ClusterPlan,
        events: mpsc::Sender<ApplyEvent>,
    ) -> ApplyOutcome {
        let started = tokio::time::Instant::now();
        let started_at = Utc::now();
        let emit = |e: ApplyEvent| {
            let events = events.clone();
            async move {
                let _ = events.send(e).await;
            }
        };
        let mut results: Vec<ModuleOutcome> = Vec::new();
        let mut failure: Option<(String, buckle_core::Error)> = None;
        let mut changes = 0;

        for unit in &planned.units {
            let module = unit.name.clone();
            if failure.is_some() {
                results.push(ModuleOutcome::new(&module, ModuleStatus::NotStarted));
                continue;
            }
            let core = planned.plan.modules.iter().find(|m| m.module == module);
            let mut steps: Vec<StepOutcome> = core
                .map(|m| {
                    m.steps
                        .iter()
                        .map(|s| {
                            let state = if s.planned.is_change() {
                                StepState::Pending
                            } else {
                                StepState::Unchanged
                            };
                            StepOutcome::planned(s, state)
                        })
                        .collect()
                })
                .unwrap_or_default();
            emit(ApplyEvent::ModuleStarted {
                module: module.clone(),
            })
            .await;
            let module_started = Utc::now();
            let module_clock = tokio::time::Instant::now();
            let mut module_changes = 0;
            let mut module_error: Option<buckle_core::Error> = None;

            if let Some((key, reason)) = unit.plan.blocked() {
                module_error = Some(buckle_core::error::user(
                    format!("{key} cannot be applied: {reason}"),
                    &["Resolve the conflict and plan again."],
                ));
            }
            for (i, step) in unit.plan.steps.iter().enumerate() {
                if module_error.is_some() || !step.is_change() {
                    continue;
                }
                let resource = resource_key(&module, &step.key);
                let id = buckle_core::engine::step_id(&resource);
                let kind = step.key.kind.task_kind().to_owned();
                emit(ApplyEvent::StepStarted {
                    module: module.clone(),
                    task_id: step.key.key(),
                    id: id.clone(),
                    kind: kind.clone(),
                })
                .await;
                let step_started = Utc::now();
                let clock = tokio::time::Instant::now();
                let applied = step.apply(&unit.cx).await;
                let outcome = steps.get_mut(i);
                match applied {
                    Ok(applied) => {
                        if applied.changed {
                            module_changes += 1;
                        }
                        emit(ApplyEvent::StepFinished {
                            module: module.clone(),
                            task_id: step.key.key(),
                            id,
                            kind,
                            changed: applied.changed,
                            summary: applied.summary.clone(),
                        })
                        .await;
                        if let Some(o) = outcome {
                            o.state = if applied.changed {
                                StepState::Changed
                            } else {
                                StepState::Unchanged
                            };
                            o.result = Some(applied.summary);
                            o.started_at = Some(step_started);
                            o.finished_at = Some(Utc::now());
                            o.duration_ms = Some(clock.elapsed().as_millis() as u64);
                        }
                    }
                    Err(e) => {
                        let report = ErrorReport::of(&e);
                        emit(ApplyEvent::StepFailed {
                            module: module.clone(),
                            task_id: step.key.key(),
                            id,
                            kind,
                            error: report.one_line(),
                            error_detail: Some(report.clone()),
                        })
                        .await;
                        if let Some(o) = outcome {
                            o.state = StepState::Failed;
                            o.started_at = Some(step_started);
                            o.finished_at = Some(Utc::now());
                            o.duration_ms = Some(clock.elapsed().as_millis() as u64);
                            o.error_detail = Some(report);
                        }
                        module_error = Some(e);
                    }
                }
            }
            let status = match &module_error {
                Some(_) => {
                    for o in steps.iter_mut().filter(|o| o.state == StepState::Pending) {
                        o.state = StepState::NotRun;
                    }
                    ModuleStatus::Failed
                }
                None if module_changes > 0 => ModuleStatus::Applied,
                None => ModuleStatus::Unchanged,
            };
            emit(ApplyEvent::ModuleFinished {
                module: module.clone(),
                status,
            })
            .await;
            changes += module_changes;
            let mut result = ModuleOutcome {
                changes: module_changes,
                duration_ms: module_clock.elapsed().as_millis() as u64,
                steps,
                started_at: Some(module_started),
                finished_at: Some(Utc::now()),
                ..ModuleOutcome::new(&module, status)
            };
            if let Some(e) = module_error {
                result = result.with_error(ErrorReport::of(&e));
                failure = Some((module, e));
            }
            results.push(result);
        }

        // Workloads that could not be planned were not applied.
        for i in &planned.plan.indeterminate {
            results.push(
                ModuleOutcome::new(&i.module, ModuleStatus::Indeterminate).with_error(
                    i.error_detail.clone().unwrap_or_else(|| {
                        ErrorReport::of(&buckle_core::error::user(i.reason.clone(), &[]))
                    }),
                ),
            );
        }

        let mut removals_run = Vec::new();
        let mut removal_error: Option<buckle_core::Error> = None;
        for ((orphan, removal, cx), planned_removal) in
            planned.removals.iter().zip(&planned.plan.removals)
        {
            let resource = planned_removal.resource.to_string();
            let mut outcome = RemovalOutcome {
                resource: resource.clone(),
                kind: planned_removal.resource.kind.clone(),
                summary: planned_removal.summary.clone(),
                state: StepState::NotRun,
                caution: planned_removal.caution,
                error_detail: None,
            };
            if failure.is_none() && removal_error.is_none() {
                match orphan.remove(cx, *removal).await {
                    Ok(applied) => {
                        outcome.state = if applied.changed {
                            StepState::Changed
                        } else {
                            StepState::Unchanged
                        };
                        outcome.summary = applied.summary.clone();
                        emit(ApplyEvent::RemovalRun {
                            resource,
                            summary: applied.summary,
                        })
                        .await;
                    }
                    Err(e) => {
                        outcome.state = StepState::Failed;
                        outcome.error_detail = Some(ErrorReport::of(&e));
                        removal_error = Some(e);
                    }
                }
            }
            removals_run.push(outcome);
        }

        let error = failure.as_ref().map(|(_, e)| e).or(removal_error.as_ref());
        let not_started: Vec<String> = results
            .iter()
            .filter(|r| r.status == ModuleStatus::NotStarted)
            .map(|r| r.module.clone())
            .collect();
        let succeeded = error.is_none() && planned.plan.indeterminate.is_empty();
        ApplyOutcome {
            plan_digest: planned.plan.digest.clone(),
            succeeded,
            changes,
            removals: removals_run
                .iter()
                .filter(|r| r.state == StepState::Changed)
                .count(),
            duration_ms: started.elapsed().as_millis() as u64,
            module_results: results,
            failed_module: failure.as_ref().map(|(m, _)| m.clone()),
            rolled_back_modules: Vec::new(),
            not_started,
            health: None,
            error: error.map(buckle_core::Error::description).or_else(|| {
                (!planned.plan.indeterminate.is_empty()).then(|| {
                    planned
                        .plan
                        .indeterminate
                        .iter()
                        .map(|i| format!("{}: {}", i.module, i.reason))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
            }),
            error_detail: error.map(ErrorReport::of),
            removals_run,
            started_at: Some(started_at),
            finished_at: Some(Utc::now()),
            reboot: None,
        }
    }

    /// Runs the implicit checks of `a`'s selected workloads (or of
    /// `modules`): each job runs at its current version with its deployment
    /// healthy, and its Nomad service checks pass. A workload's extra
    /// `health` checks are reported as not run.
    pub async fn health(
        &self,
        a: &Assignment,
        scope: HealthScope,
        modules: Option<&[String]>,
        rollout: Option<&str>,
    ) -> buckle_core::Result<HealthReport> {
        let nomad = self.client(a, Purpose::Read, rollout).await?;
        let ctx = a.context();
        let cx = Context::new(
            nomad.clone(),
            &a.target,
            "",
            &a.revision.id.0,
            a.spec.fingerprints.clone(),
        );
        let mut jobs: HashMap<String, Vec<ObjectKey>> = HashMap::new();
        for owned in buckle_nomad::primitives::orphans(&cx, &[]).await? {
            if owned.key.kind == Kind::Job {
                jobs.entry(owned.module).or_default().push(owned.key);
            }
        }
        let mut results = Vec::new();
        for w in a.workloads(&ctx) {
            let module = &w.config.name;
            if modules.is_some_and(|m| !m.contains(module)) {
                continue;
            }
            let result =
                |check: &str, kind: &str, required: bool, status: HealthStatus| HealthResult {
                    module: module.clone(),
                    check: check.to_owned(),
                    kind: kind.to_owned(),
                    required,
                    status,
                    at: Utc::now(),
                    duration_ms: 0,
                };
            let owned = jobs.remove(module).unwrap_or_default();
            if w.config.job.is_some() && owned.is_empty() {
                results.push(result(
                    "job",
                    "nomad.job",
                    true,
                    HealthStatus::fail(format!("the job of {module} is not registered")),
                ));
            }
            for key in owned {
                let started = tokio::time::Instant::now();
                for check in buckle_nomad::health::job(&nomad, &key.namespace, &key.name).await? {
                    let mut r = result(check.name, "nomad.job", true, check.status);
                    r.duration_ms = started.elapsed().as_millis() as u64;
                    results.push(r);
                }
            }
            for decl in &w.config.health {
                results.push(result(
                    decl.display_name(),
                    &decl.check.kind,
                    decl.required,
                    HealthStatus::unknown("extra checks on workloads are not run by executors yet"),
                ));
            }
        }
        Ok(HealthReport { scope, results })
    }
}

/// Exchanges a Buckle-issued JWT for a Nomad ACL token through the cluster's
/// own client, so a Unix socket API works too.
async fn nomad_login(
    nomad: &Nomad,
    target: &str,
    identity: &crate::identity::IssuedIdentity,
) -> buckle_core::Result<Credential> {
    #[derive(serde::Deserialize)]
    struct Token {
        #[serde(rename = "AccessorID")]
        accessor_id: String,
        #[serde(rename = "SecretID")]
        secret_id: String,
        #[serde(rename = "ExpirationTime", default)]
        expiration_time: Option<chrono::DateTime<Utc>>,
    }
    let token: Token = nomad
        .write(
            "POST",
            "/v1/acl/login",
            &serde_json::json!({
                "AuthMethodName": Login::DEFAULT_NOMAD_AUTH_METHOD,
                "LoginToken": identity.token,
            }),
        )
        .await
        .map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("Could not log in to Nomad for {target}."),
                &[
                    "Check the cluster's trust setup: a `jwt` auth method named `buckle` trusting Buckle's keys, and a binding rule for this purpose.",
                ],
            )
        })?;
    tracing::debug!(accessor = %token.accessor_id, %target, "logged in to Nomad");
    Ok(Credential::Nomad {
        secret_id: token.secret_id,
        accessor_id: token.accessor_id,
        expires_at: token.expiration_time.unwrap_or(identity.expires_at),
    })
}

/// The workloads in `needs` order: each after the selected workloads it
/// needs, otherwise in the revision's order. `module:` edges are the
/// rollout's to order (stages); a cycle (rejected by validation) keeps the
/// revision's order for the rest.
fn order(selected: Vec<&LoadedWorkload>) -> Vec<&LoadedWorkload> {
    let names: BTreeSet<&str> = selected.iter().map(|w| w.config.name.as_str()).collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut rest = selected;
    while !rest.is_empty() {
        let before = rest.len();
        let mut later = Vec::new();
        for w in rest {
            let ready = w.config.needs().iter().all(|n| match n {
                buckle_core::model::Need::Workload(dep) => {
                    !names.contains(dep.as_str()) || done.contains(dep)
                }
                buckle_core::model::Need::Module(_) => true,
            });
            if ready {
                done.insert(w.config.name.clone());
                out.push(w);
            } else {
                later.push(w);
            }
        }
        if later.len() == before {
            out.extend(later);
            break;
        }
        rest = later;
    }
    out
}

/// A workload's `namespace` that the cluster lacks is created by the first
/// workload (in apply order) naming it; one that exists is left alone.
struct Implicit {
    missing: BTreeSet<String>,
    /// Namespaces that exist: declared, so a stamped one is never an orphan.
    present: Vec<ObjectKey>,
}

async fn implicit_namespaces(
    nomad: &Nomad,
    workloads: &[&LoadedWorkload],
) -> buckle_core::Result<Implicit> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for w in workloads {
        if let Some(ns) = w.config.namespace.as_deref()
            && !ns.is_empty()
            && ns != "default"
            && !w.config.uses_kubernetes_keys()
        {
            wanted.insert(ns.to_owned());
        }
    }
    let mut out = Implicit {
        missing: BTreeSet::new(),
        present: Vec::new(),
    };
    for ns in wanted {
        let path = format!("/v1/namespace/{}", buckle_nomad::client::seg(&ns));
        match nomad.get::<serde_json::Value>(&path).await? {
            Some(_) => out.present.push(ObjectKey::new(Kind::Namespace, "", &ns)),
            None => {
                out.missing.insert(ns);
            }
        }
    }
    Ok(out)
}

fn implicit_namespace(ns: &str) -> EntitySpec {
    EntitySpec {
        kind: EntityKind::Namespace,
        file: "workload.yaml namespace".to_owned(),
        hcl: format!("name = {}\n", serde_json::Value::String(ns.to_owned())),
    }
}

/// `workload.yaml`'s Nomad keys with its files read, its `.j2` files and
/// vars rendered and its variable items rendered (with secret values, kept
/// sensitive).
async fn compile(
    revision: &Revision,
    w: &LoadedWorkload,
    ctx: &HostContext,
) -> buckle_core::Result<Workload> {
    let cfg = &w.config;
    let ctx = ctx.in_module(&cfg.name, &cfg.tags);
    let mut templater = Templater::new();
    for (path, source) in &w.templates {
        templater.add_template(path.to_string_lossy().into_owned(), source.clone())?;
    }
    let raw = buckle_core::engine::layer_vars_over(
        revision,
        &cfg.vars,
        &buckle_core::selectors::Vars::new(),
        &ctx,
    )?;
    let vars = buckle_core::engine::render_vars(raw, &cfg.name, &templater, &ctx)?;
    let ctx = ctx.with_vars(vars.clone());

    let read = |file: String| {
        let templater = &templater;
        let ctx = &ctx;
        async move {
            let bytes = w.source.read(Path::new(&file)).await?.ok_or_else(|| {
                buckle_core::error::user(
                    format!(
                        "The workload {} names {file}, which is not under its files/.",
                        cfg.name
                    ),
                    &["Run `buckle validate` to list the missing files."],
                )
            })?;
            let text = String::from_utf8(bytes).map_err(|_| {
                buckle_core::error::user(
                    format!("{file} of workload {} is not UTF-8 text.", cfg.name),
                    &["Workload files are text (HCL, YAML, templates)."],
                )
            })?;
            if file.ends_with(".j2") {
                templater.render_plain(&text, ctx).map_err(|e| {
                    buckle_core::error::wrap_user(
                        e,
                        format!("{file} of workload {} could not be rendered.", cfg.name),
                        &["Secrets reach a Nomad job through `variables`, never its spec."],
                    )
                })
            } else {
                Ok(text)
            }
        }
    };

    let job = match &cfg.job {
        Some(decl) => {
            let mut files = IndexMap::new();
            for (var, file) in &cfg.files {
                files.insert(var.clone(), read(file.clone()).await?);
            }
            Some(JobSpec {
                file: decl.file.clone(),
                hcl: read(decl.file.clone()).await?,
                vars: vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                files,
                namespace: cfg.namespace.clone(),
            })
        }
        None => None,
    };
    let mut volumes = Vec::new();
    for file in &cfg.volumes {
        volumes.push(VolumeSpec {
            file: file.clone(),
            hcl: read(file.clone()).await?,
        });
    }
    let mut policies = Vec::new();
    for p in &cfg.policies {
        policies.push(PolicySpec {
            file: p.file.clone(),
            rules: read(p.file.clone()).await?,
            name: None,
            description: cfg.description.clone(),
            attach: p.attach.as_ref().map(|a| Attach {
                namespace: a.namespace.clone().or_else(|| cfg.namespace.clone()),
                job: a.job.clone(),
                group: a.group.clone(),
                task: a.task.clone(),
            }),
        });
    }
    let entity = |kind: EntityKind, file: &String| {
        let read = &read;
        let file = file.clone();
        async move {
            Ok::<_, buckle_core::Error>(EntitySpec {
                kind,
                hcl: read(file.clone()).await?,
                file,
            })
        }
    };
    let mut namespaces = Vec::new();
    for file in &cfg.namespaces {
        namespaces.push(entity(EntityKind::Namespace, file).await?);
    }
    let mut node_pools = Vec::new();
    for file in &cfg.node_pools {
        node_pools.push(entity(EntityKind::NodePool, file).await?);
    }
    let mut resources = Vec::new();
    for r in &cfg.resources {
        resources.push(entity(EntityKind::resource(&r.kind)?, &r.file).await?);
    }
    let namespace = cfg
        .namespace
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let mut variables = Vec::new();
    for (path, items) in &cfg.variables {
        let mut rendered = IndexMap::new();
        for (key, value) in items {
            let item_ctx = ctx.in_task("nomad.variable", path, &[]);
            rendered.insert(
                key.clone(),
                templater.render_sensitive(value, &item_ctx).map_err(|e| {
                    buckle_core::error::wrap_user(
                        e,
                        format!(
                            "The item {key} of variable {path} (workload {}) could not be rendered.",
                            cfg.name
                        ),
                        &["Check the template and that the secrets it reads are declared."],
                    )
                })?,
            );
        }
        variables.push(VariableSpec {
            namespace: namespace.clone(),
            path: path.clone(),
            items: rendered,
        });
    }
    Ok(Workload {
        name: cfg.name.clone(),
        job,
        volumes,
        variables,
        policies,
        namespaces,
        node_pools,
        resources,
        removal: nomad_removal(cfg.removal),
    })
}

fn nomad_removal(removal: WorkloadRemoval) -> buckle_nomad::Removal {
    match removal {
        WorkloadRemoval::Stop => buckle_nomad::Removal::Stop,
        WorkloadRemoval::Purge => buckle_nomad::Removal::Purge,
        WorkloadRemoval::Keep => buckle_nomad::Removal::Keep,
    }
}

/// The plan's resource key of a cluster object: `paste/nomad.job[default/paste]`.
fn resource_key(module: &str, key: &ObjectKey) -> ResourceKey {
    ResourceKey::new(module, key.kind.task_kind(), key.key())
}

/// A workload's steps as a module plan.
fn module_plan(name: &str, needs: &[String], plan: &WorkloadPlan) -> ModulePlan {
    ModulePlan {
        module: name.to_owned(),
        needs: needs.to_vec(),
        steps: plan
            .steps
            .iter()
            .map(|s| Step {
                task: TaskRef {
                    module: name.to_owned(),
                    id: s.key.key(),
                    kind: s.key.kind.task_kind().to_owned(),
                    name: None,
                },
                resource: resource_key(name, &s.key),
                planned: s.planned.clone(),
                locks: Vec::new(),
                notifies: Vec::new(),
                always_runs: false,
                removal: RemovalPolicy::Cleanup,
            })
            .collect(),
        handlers: Vec::new(),
        provides_facts: Vec::new(),
    }
}

/// Keeps the most recent assignments per target, so a rollback or a health
/// request for an earlier revision still finds its secrets.
#[derive(Debug, Default)]
pub struct Assignments {
    by_target: BTreeMap<String, Vec<Assignment>>,
}

impl Assignments {
    /// How many revisions are kept per target.
    pub const KEEP: usize = 4;

    pub fn insert(&mut self, a: Assignment) {
        let list = self.by_target.entry(a.target.clone()).or_default();
        list.retain(|b| b.revision.id != a.revision.id);
        list.push(a);
        if list.len() > Self::KEEP {
            list.remove(0);
        }
    }

    pub fn get(&self, target: &str, revision: &RevisionId) -> Option<&Assignment> {
        self.by_target
            .get(target)?
            .iter()
            .find(|a| &a.revision.id == revision)
    }

    /// The revisions held for any target, so the host's pruning keeps them.
    pub fn revisions(&self) -> impl Iterator<Item = &RevisionId> {
        self.by_target.values().flatten().map(|a| &a.revision.id)
    }
}

#[cfg(test)]
mod tests;
