//! The agent's second pipeline: cluster targets it executes for (design:
//! cluster workloads, D6 and D7).
//!
//! * The server picks this host as a cluster's executor and sends a targeted
//!   Assign ([`Assignment`]) ahead of every plan, apply, rollback and health
//!   request for it, so nothing about the cluster is kept between them.
//! * [`Executor`] compiles the revision's selected workloads into
//!   [`buckle_nomad`] or [`buckle_kube`] primitives, plans them into an ordinary
//!   [`Plan`] (one module per workload), applies them concurrently as
//!   `needs` allow and runs their implicit health checks. Ownership lives in the cluster
//!   (stamps and the index variable); objects of workloads that went away
//!   are planned as removals.
//! * Credentials come from Buckle-issued identities ([`crate::identity`]),
//!   the host's own credential (`local`: `NOMAD_TOKEN` or a kubeconfig;
//!   `workload`: the task's token or the pod's ServiceAccount) or a
//!   `secret://` secret.

mod kubernetes;

use crate::identity::{Credential, Credentials, IdentityRequest, Login, Purpose};
use crate::transport::Transport;
use buckle_core::engine::{
    ApplyEvent, ApplyOutcome, LoadedWorkload, ModuleOutcome, ModuleStatus, RemovalOutcome,
    Revision, StepOutcome, StepState,
};
use buckle_core::error::ErrorReport;
use buckle_core::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use buckle_core::model::{
    ClusterIdentity, ClusterKind, RemovalPolicy, ResourceEntry, ResourceKind, WorkloadRemoval,
};
use buckle_core::plan::{
    Indeterminate, ModulePlan, Plan, Removal, ResourceKey, RevisionId, Step, TaskRef,
};
use buckle_core::secrets::sinks::ApiSink;
use buckle_core::secrets::{InstallKey, Rendered, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity};
use buckle_core::template::Templater;
use buckle_nomad::primitives::{
    Attach, EntityKind, EntitySpec, JobSpec, Orphan, PolicySpec, Primitive, VariableSpec,
    VolumeSpec,
};
use buckle_nomad::{Context, Kind, Nomad, ObjectKey, Workload, WorkloadPlan};
use chrono::Utc;
use futures::stream::{FuturesUnordered, StreamExt};
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

    /// A Kubernetes client carrying the host's own credential where the
    /// identity uses one (`local`, `workload`), else only the address and CA.
    fn kubernetes(
        &self,
        _target: &str,
        spec: &TargetSpec,
    ) -> buckle_core::Result<buckle_kube::Kube> {
        buckle_kube::Kube::connect(kubernetes::endpoint(spec)?)
    }
}

/// Connects over the network (or a Unix socket) to the cluster's `api`.
#[derive(Debug, Default)]
pub struct Network;

impl Connect for Network {
    fn nomad(&self, _target: &str, spec: &TargetSpec) -> buckle_core::Result<Nomad> {
        Nomad::connect(&nomad_address(spec), None)
    }
}

/// The Nomad API a target is reached at: its `api`, else `NOMAD_ADDR` (the
/// other way round for a `workload` identity), else
/// the local agent.
pub fn nomad_address(spec: &TargetSpec) -> String {
    let env = std::env::var("NOMAD_ADDR").ok().filter(|a| !a.is_empty());
    let api = spec.api.clone().filter(|a| !a.is_empty());
    // Inside the cluster the scheduler's address (its Task API socket) is
    // the one that takes the workload's token; `api` is how hosts reach it.
    let ordered = if spec.identity == ClusterIdentity::Workload {
        env.or(api)
    } else {
        api.or(env)
    };
    ordered.unwrap_or_else(|| DEFAULT_NOMAD_ADDR.to_owned())
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

/// How many workloads of one target apply at once. A job's step lasts
/// until its deployment finishes, so the cap bounds open deployments (and
/// their polling), not CPU.
const PARALLEL_WORKLOADS: usize = 8;

/// One workload's part of a plan.
struct Unit {
    name: String,
    plan: UnitPlan,
    /// The workloads that must apply first: the selected ones it `needs`
    /// (planned or not) and earlier ones creating a namespace, node pool or
    /// resource, which it may land in.
    after: BTreeSet<String>,
}

/// A cluster plan: what the server sees ([`ClusterPlan::plan`]) and what
/// the apply sends.
pub struct ClusterPlan {
    pub plan: Plan,
    units: Vec<Unit>,
    removals: Vec<RemovalStep>,
}

/// A workload's plan on its scheduler, with the context its apply uses.
enum UnitPlan {
    Nomad(Context, WorkloadPlan),
    Kube(buckle_kube::Context, buckle_kube::WorkloadPlan),
}

/// What the plan shows of one step: its resource key, kind and whether it
/// changes anything.
struct StepView {
    key: String,
    kind: &'static str,
    change: bool,
}

impl UnitPlan {
    fn steps(&self) -> Vec<StepView> {
        match self {
            UnitPlan::Nomad(_, p) => p
                .steps
                .iter()
                .map(|s| StepView {
                    key: s.key.key(),
                    kind: s.key.kind.task_kind(),
                    change: s.is_change(),
                })
                .collect(),
            UnitPlan::Kube(_, p) => p
                .steps
                .iter()
                .map(|s| StepView {
                    key: s.key.key(),
                    kind: s.task_kind(),
                    change: s.is_change(),
                })
                .collect(),
        }
    }

    fn blocked(&self) -> Option<String> {
        match self {
            UnitPlan::Nomad(_, p) => p
                .blocked()
                .map(|(k, r)| format!("{k} cannot be applied: {r}")),
            UnitPlan::Kube(_, p) => p
                .blocked()
                .map(|(k, r)| format!("{k} cannot be applied: {r}")),
        }
    }

    /// Applies step `i`, reporting its progress (a rollout's healthy
    /// allocations or replicas) through `progress`.
    async fn apply(
        &self,
        i: usize,
        progress: buckle_nomad::primitives::Progress,
    ) -> buckle_core::Result<buckle_core::task::Applied> {
        match self {
            UnitPlan::Nomad(cx, p) => p.steps[i].apply(&cx.clone().with_progress(progress)).await,
            UnitPlan::Kube(cx, p) => p.steps[i].apply(&cx.clone().with_progress(progress)).await,
        }
    }
}

/// An orphan to remove, on its scheduler.
enum RemovalStep {
    Nomad(Orphan, buckle_nomad::Removal, Context),
    Kube(
        buckle_kube::Orphan,
        buckle_kube::Removal,
        buckle_kube::Context,
    ),
}

impl RemovalStep {
    async fn remove(&self) -> buckle_core::Result<buckle_core::task::Applied> {
        match self {
            RemovalStep::Nomad(o, r, cx) => o.remove(cx, *r).await,
            RemovalStep::Kube(o, r, cx) => o.remove(cx, *r).await,
        }
    }
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

    /// A Nomad client for `a`'s cluster carrying a credential for `purpose`.
    async fn client(
        &self,
        a: &Assignment,
        purpose: Purpose,
        rollout: Option<&str>,
    ) -> buckle_core::Result<Nomad> {
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

    /// A Nomad client for `a`'s cluster with a `read` credential: what a
    /// lease holder's watch reads with. The credential cache renews it as
    /// it nears expiry.
    pub(crate) async fn read_nomad(&self, a: &Assignment) -> buckle_core::Result<Nomad> {
        self.client(a, Purpose::Read, None).await
    }

    /// A Kubernetes client for `a`'s cluster with a `read` credential, and
    /// when the credential expires (`None` for a credential of the host's
    /// own that does not): a watch re-syncs when the identity is re-issued.
    pub(crate) async fn read_kube(
        &self,
        a: &Assignment,
    ) -> buckle_core::Result<(buckle_kube::Kube, Option<chrono::DateTime<Utc>>)> {
        self.kube_client_with_expiry(a, Purpose::Read, None).await
    }

    /// Whether the cluster's API answers at all, without a credential (a
    /// lease holder's probe while nothing is applied yet): Nomad's leader
    /// and Kubernetes' version endpoints answer anonymous callers.
    pub(crate) async fn probe(&self, target: &str, spec: &TargetSpec) -> buckle_core::Result<()> {
        match spec.kind {
            ClusterKind::Nomad => {
                let nomad = self.connect.nomad(target, spec)?;
                let resp = nomad.send("GET", "/v1/status/leader", None).await?;
                nomad.ok("GET", "/v1/status/leader", resp).map(|_| ())
            }
            ClusterKind::Kubernetes => {
                let kube = self.connect.kubernetes(target, spec)?;
                let resp = kube.send("GET", "/version", None, None).await?;
                kube.ok("GET", "/version", resp).map(|_| ())
            }
        }
    }

    /// Plans `a`. Pure: reads the cluster, changes nothing. A workload that
    /// cannot be planned is indeterminate (its objects are never orphans);
    /// one that is unreachable fails the plan.
    ///
    /// With a `scope`, plans that one workload placement: the workload named
    /// (nothing, when the revision no longer selects it on the cluster) and
    /// the removal of what that workload owns beyond its plan; other
    /// workloads' objects are not its orphans.
    ///
    /// Plans with a `deploy` credential: Nomad's job plan needs `submit-job`
    /// (it has no plan-only capability), which leaves `read` read-only.
    pub async fn plan(
        &self,
        a: &Assignment,
        rollout: Option<&str>,
        scope: Option<&str>,
    ) -> buckle_core::Result<ClusterPlan> {
        if a.spec.kind == ClusterKind::Kubernetes {
            return self.plan_kubernetes(a, rollout, scope).await;
        }
        let nomad = self.client(a, Purpose::Deploy, rollout).await?;
        let ctx = a.context();
        let workloads = scoped(a.workloads(&ctx), scope);
        // ACL policies need a management token, which only workloads that
        // list one ask for; the cluster binds `acl` to it.
        let acl = if workloads
            .iter()
            .any(|w| w.config.of_kind(ResourceKind::Policy).next().is_some())
        {
            Some(self.client(a, Purpose::Acl, rollout).await?)
        } else {
            None
        };
        let implicit = implicit_namespaces(&nomad, &workloads).await?;

        let mut units = Vec::new();
        let mut modules = Vec::new();
        let mut warnings = Vec::new();
        let mut indeterminate = Vec::new();
        let mut declared: Vec<ObjectKey> = implicit.present.clone();
        let mut undecided: BTreeSet<String> = BTreeSet::new();
        let mut earlier: Vec<ObjectKey> = Vec::new();
        let mut namespaces_created: BTreeSet<String> = BTreeSet::new();
        let selected: BTreeSet<String> = workloads.iter().map(|w| w.config.name.clone()).collect();
        // Earlier workloads that create cluster-wide objects others land in.
        let mut landing: BTreeSet<String> = BTreeSet::new();

        for w in workloads {
            let name = w.config.name.clone();
            let mut after: BTreeSet<String> = w
                .config
                .needs()
                .into_iter()
                .filter_map(|n| match n {
                    buckle_core::model::Need::Workload(dep) if selected.contains(&dep) => Some(dep),
                    _ => None,
                })
                .collect();
            after.extend(landing.iter().cloned());
            let emitted = std::sync::Arc::new(buckle_core::template::Emitted::default());
            let compiled = if w.config.uses_kubernetes_keys() {
                Err(buckle_core::error::user(
                    format!(
                        "The workload {name} uses Kubernetes keys, but {} is a Nomad cluster.",
                        a.target
                    ),
                    &["Narrow the workload's `when` to `cluster.kind == \"kubernetes\"`."],
                ))
            } else {
                // The workload's renders are traced, so the fleet view marks
                // only the cluster values its templates emitted.
                compile(&a.revision, w, &ctx.clone().with_emitted(emitted.clone())).await
            };
            let planned = match compiled {
                Ok(mut workload) => {
                    if let Some(ns) = w.config.namespace.as_deref()
                        && implicit.missing.contains(ns)
                        && namespaces_created.insert(ns.to_owned())
                    {
                        workload
                            .primitives
                            .insert(0, Primitive::Entity(implicit_namespace(ns)));
                    }
                    let cx = Context::new(
                        nomad.clone(),
                        &a.target,
                        &name,
                        &a.revision.id.0,
                        a.spec.fingerprints.clone(),
                    )
                    .with_acl(acl.clone());
                    workload
                        .plan_after(&cx, &earlier)
                        .await
                        .map(|plan| (cx, plan))
                }
                Err(e) => Err(e),
            };
            match planned {
                Ok((cx, plan)) => {
                    let created = plan.created();
                    if created.iter().any(|k| {
                        matches!(k.kind, Kind::Namespace | Kind::NodePool | Kind::Resource)
                    }) {
                        landing.insert(name.clone());
                    }
                    earlier.extend(created);
                    declared.extend(plan.keys());
                    for step in &plan.steps {
                        warnings.extend(
                            step.warnings
                                .iter()
                                .map(|m| format!("{name}: {}: {m}", step.key)),
                        );
                    }
                    modules.push(module_plan(
                        &name,
                        &w.config.needs,
                        emitted.paths(),
                        plan.steps
                            .iter()
                            .map(|s| (s.key.key(), s.key.kind.task_kind(), s.planned.clone())),
                    ));
                    units.push(Unit {
                        name,
                        plan: UnitPlan::Nomad(cx, plan),
                        after,
                    });
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
        )
        .with_acl(acl);
        let mut removals = Vec::new();
        let mut planned_removals = Vec::new();
        for orphan in buckle_nomad::primitives::orphans(&cx, &declared).await? {
            // A workload whose plan failed declares nothing we know of, and
            // a placement removes only what its own workload owned.
            if undecided.contains(&orphan.module) || scope.is_some_and(|s| s != orphan.module) {
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
                resource: resource_key(
                    &orphan.module,
                    orphan.key.kind.task_kind(),
                    &orphan.key.key(),
                ),
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
            removals.push(RemovalStep::Nomad(orphan, removal, ocx));
        }

        Ok(finish(
            a,
            modules,
            planned_removals,
            indeterminate,
            warnings,
            units,
            removals,
        ))
    }

    /// Applies `planned`'s workloads as their `needs` allow, several at once
    /// ([`PARALLEL_WORKLOADS`]); after the first failure nothing more starts
    /// (running ones finish). Then its removals. Events stream to `events`;
    /// the outcome says what happened.
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
        let mut outcomes: Vec<Option<ModuleOutcome>> = vec![None; planned.units.len()];
        let mut failure: Option<(String, buckle_core::Error)> = None;
        let mut changes = 0;
        let mut applied: BTreeSet<&str> = BTreeSet::new();
        let mut running = FuturesUnordered::new();
        let mut waiting: Vec<usize> = (0..planned.units.len()).collect();

        loop {
            if failure.is_none() {
                waiting.retain(|&i| {
                    let unit = &planned.units[i];
                    let ready = running.len() < PARALLEL_WORKLOADS
                        && unit.after.iter().all(|d| applied.contains(d.as_str()));
                    if ready {
                        let events = &events;
                        running.push(async move { (i, apply_unit(planned, unit, events).await) });
                    }
                    !ready
                });
            }
            let Some((i, (outcome, error))) = running.next().await else {
                break;
            };
            changes += outcome.changes;
            match error {
                Some(e) => {
                    failure.get_or_insert((outcome.module.clone(), e));
                }
                None => {
                    applied.insert(planned.units[i].name.as_str());
                }
            }
            outcomes[i] = Some(outcome);
        }
        // Not started: after a failure, or needing a workload that failed or
        // could not be planned.
        let mut results: Vec<ModuleOutcome> = outcomes
            .into_iter()
            .zip(&planned.units)
            .map(|(o, u)| {
                o.unwrap_or_else(|| ModuleOutcome::new(&u.name, ModuleStatus::NotStarted))
            })
            .collect();

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
        for (step, planned_removal) in planned.removals.iter().zip(&planned.plan.removals) {
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
                match step.remove().await {
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

    /// Runs `action` of the selected workload `workload` on `a`'s cluster:
    /// an implicit action (`restart`, `stop`, `revert`) on each job the
    /// workload owns there, or one of its `actions:` (`dispatch`). One step
    /// per job; the first failure stops the rest. Nothing is planned or
    /// recorded.
    pub async fn run_action(
        &self,
        a: &Assignment,
        workload: &str,
        action: &str,
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
        let resolved = async {
            let nomad = self.client(a, Purpose::Deploy, None).await?;
            let steps = action_steps(&nomad, a, workload, action).await?;
            Ok::<_, buckle_core::Error>((nomad, steps))
        }
        .await;
        let (nomad, steps) = match resolved {
            Ok(r) => r,
            Err(e) => return action_failed(&e, started, started_at),
        };
        emit(ApplyEvent::ModuleStarted {
            module: workload.to_owned(),
        })
        .await;
        let mut outcomes = Vec::new();
        let mut failure: Option<buckle_core::Error> = None;
        let mut changes = 0;
        for step in &steps {
            let resource = resource_key(workload, step.key.kind.task_kind(), &step.key.key());
            let id = buckle_core::engine::step_id(&resource);
            let kind = step.key.kind.task_kind().to_owned();
            let mut outcome = StepOutcome {
                id: id.clone(),
                kind: kind.clone(),
                name: None,
                summary: step.summary.clone(),
                state: StepState::NotRun,
                result: None,
                started_at: None,
                finished_at: None,
                duration_ms: None,
                error_detail: None,
            };
            if failure.is_some() {
                outcomes.push(outcome);
                continue;
            }
            emit(ApplyEvent::StepStarted {
                module: workload.to_owned(),
                task_id: step.key.key(),
                id: id.clone(),
                kind: kind.clone(),
            })
            .await;
            outcome.started_at = Some(Utc::now());
            let clock = tokio::time::Instant::now();
            let ran = step.run(&nomad).await;
            outcome.finished_at = Some(Utc::now());
            outcome.duration_ms = Some(clock.elapsed().as_millis() as u64);
            match ran {
                Ok(summary) => {
                    changes += 1;
                    emit(ApplyEvent::StepFinished {
                        module: workload.to_owned(),
                        task_id: step.key.key(),
                        id,
                        kind,
                        changed: true,
                        summary: summary.clone(),
                    })
                    .await;
                    outcome.state = StepState::Changed;
                    outcome.result = Some(summary);
                }
                Err(e) => {
                    let report = ErrorReport::of(&e);
                    emit(ApplyEvent::StepFailed {
                        module: workload.to_owned(),
                        task_id: step.key.key(),
                        id,
                        kind,
                        error: report.one_line(),
                        error_detail: Some(report.clone()),
                    })
                    .await;
                    outcome.state = StepState::Failed;
                    outcome.error_detail = Some(report);
                    failure = Some(e);
                }
            }
            outcomes.push(outcome);
        }
        let status = match (&failure, changes) {
            (Some(_), _) => ModuleStatus::Failed,
            (None, 0) => ModuleStatus::Unchanged,
            (None, _) => ModuleStatus::Applied,
        };
        emit(ApplyEvent::ModuleFinished {
            module: workload.to_owned(),
            status,
        })
        .await;
        let mut result = ModuleOutcome {
            changes,
            duration_ms: started.elapsed().as_millis() as u64,
            steps: outcomes,
            started_at: Some(started_at),
            finished_at: Some(Utc::now()),
            ..ModuleOutcome::new(workload, status)
        };
        if let Some(e) = &failure {
            result = result.with_error(ErrorReport::of(e));
        }
        ApplyOutcome {
            plan_digest: buckle_core::Digest::of(
                format!("{}/{workload}/{action}", a.target).as_bytes(),
            ),
            succeeded: failure.is_none(),
            changes,
            removals: 0,
            duration_ms: started.elapsed().as_millis() as u64,
            module_results: vec![result],
            failed_module: failure.is_some().then(|| workload.to_owned()),
            rolled_back_modules: Vec::new(),
            not_started: Vec::new(),
            health: None,
            error: failure
                .as_ref()
                .map(|e| format!("workload '{workload}': {}", e.description())),
            error_detail: None,
            removals_run: Vec::new(),
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
        if a.spec.kind == ClusterKind::Kubernetes {
            return self.health_kubernetes(a, scope, modules, rollout).await;
        }
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
            if w.config.jobs().next().is_some() && owned.is_empty() {
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

/// One job a workload action runs on.
struct ActionStep {
    key: ObjectKey,
    summary: String,
    run: ActionRun,
}

enum ActionRun {
    Job(buckle_nomad::actions::Action),
    Dispatch {
        meta: BTreeMap<String, String>,
        payload: Option<String>,
    },
}

impl ActionStep {
    async fn run(&self, nomad: &Nomad) -> buckle_core::Result<String> {
        let (ns, id) = (&self.key.namespace, &self.key.name);
        match &self.run {
            ActionRun::Job(action) => buckle_nomad::actions::run(nomad, ns, id, action).await,
            ActionRun::Dispatch { meta, payload } => {
                buckle_nomad::actions::dispatch(nomad, ns, id, meta, payload.as_deref()).await
            }
        }
    }
}

/// What `action` of `workload` runs on: each of the workload's jobs in the
/// cluster for an implicit action, the named job for a `dispatch`.
async fn action_steps(
    nomad: &Nomad,
    a: &Assignment,
    workload: &str,
    action: &str,
) -> buckle_core::Result<Vec<ActionStep>> {
    use buckle_nomad::actions::Action;
    let ctx = a.context();
    let Some(w) = a
        .workloads(&ctx)
        .into_iter()
        .find(|w| w.config.name == workload)
    else {
        return Err(buckle_core::error::user(
            format!(
                "The workload {workload} is not selected on {} at revision {}.",
                a.target,
                a.revision.id.short()
            ),
            &["Run actions of the workloads the cluster's page lists."],
        ));
    };
    if let Some(implicit) = w
        .config
        .implicit_actions()
        .iter()
        .find(|i| i.name == action)
    {
        let job = match implicit.name {
            "restart" => Action::Restart,
            "stop" => Action::Stop,
            _ => Action::Revert,
        };
        let cx = Context::new(
            nomad.clone(),
            &a.target,
            "",
            &a.revision.id.0,
            a.spec.fingerprints.clone(),
        );
        let steps: Vec<ActionStep> = buckle_nomad::primitives::orphans(&cx, &[])
            .await?
            .into_iter()
            .filter(|o| o.module == workload && o.key.kind == Kind::Job)
            .map(|o| ActionStep {
                summary: format!("{} job {}", implicit.label.to_lowercase(), o.key.key()),
                key: o.key,
                run: ActionRun::Job(job.clone()),
            })
            .collect();
        if steps.is_empty() {
            return Err(buckle_core::error::user(
                format!(
                    "The workload {workload} has no job registered on {}.",
                    a.target
                ),
                &["Apply the workload first."],
            ));
        }
        return Ok(steps);
    }
    let Some(decl) = w.config.actions.get(action) else {
        return Err(buckle_core::error::user(
            format!("The workload {workload} declares no action '{action}'."),
            &["Run one of the actions the cluster's page lists for it."],
        ));
    };
    let spec: buckle_core::model::DispatchSpec = serde_yaml::from_value(decl.task.spec.clone())
        .map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("The action {action} of workload {workload} is invalid."),
                &["Run `buckle validate`."],
            )
        })?;
    let (templater, ctx, _) = workload_templating(&a.revision, w, &ctx)?;
    let render = |text: &str| {
        templater.render_plain(text, &ctx).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("The action {action} of workload {workload} could not be rendered."),
                &["Secrets reach a Nomad job through `variables`, never its dispatch."],
            )
        })
    };
    let ns = match &spec.namespace {
        Some(ns) => render(ns)?,
        None => w
            .config
            .namespace
            .clone()
            .unwrap_or_else(|| "default".into()),
    };
    let job = render(&spec.job)?;
    let meta = spec
        .meta
        .iter()
        .map(|(k, v)| Ok((k.clone(), render(v)?)))
        .collect::<buckle_core::Result<BTreeMap<_, _>>>()?;
    let payload = spec.payload.as_deref().map(render).transpose()?;
    Ok(vec![ActionStep {
        summary: format!("dispatch job {ns}/{job}"),
        key: ObjectKey::new(Kind::Job, ns, job),
        run: ActionRun::Dispatch { meta, payload },
    }])
}

/// The outcome of an action that failed before it started.
fn action_failed(
    error: &buckle_core::Error,
    started: tokio::time::Instant,
    started_at: chrono::DateTime<Utc>,
) -> ApplyOutcome {
    ApplyOutcome {
        plan_digest: buckle_core::Digest::of(b""),
        succeeded: false,
        changes: 0,
        removals: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        module_results: Vec::new(),
        failed_module: None,
        rolled_back_modules: Vec::new(),
        not_started: Vec::new(),
        health: None,
        error: Some(error.description()),
        error_detail: Some(ErrorReport::of(error)),
        removals_run: Vec::new(),
        started_at: Some(started_at),
        finished_at: Some(Utc::now()),
        reboot: None,
    }
}

/// Applies one workload's steps in order, stopping at the first failure.
async fn apply_unit(
    planned: &ClusterPlan,
    unit: &Unit,
    events: &mpsc::Sender<ApplyEvent>,
) -> (ModuleOutcome, Option<buckle_core::Error>) {
    let emit = |e: ApplyEvent| async move {
        let _ = events.send(e).await;
    };
    let module = unit.name.clone();
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

    if let Some(reason) = unit.plan.blocked() {
        module_error = Some(buckle_core::error::user(
            reason,
            &["Resolve the conflict and plan again."],
        ));
    }
    for (i, step) in unit.plan.steps().into_iter().enumerate() {
        if module_error.is_some() || !step.change {
            continue;
        }
        let resource = resource_key(&module, step.kind, &step.key);
        let id = buckle_core::engine::step_id(&resource);
        let kind = step.kind.to_owned();
        emit(ApplyEvent::StepStarted {
            module: module.clone(),
            task_id: step.key.clone(),
            id: id.clone(),
            kind: kind.clone(),
        })
        .await;
        let step_started = Utc::now();
        let clock = tokio::time::Instant::now();
        // A step's progress (a deployment's healthy allocations) goes out
        // as it happens; one the channel has no room for is dropped, as the
        // next replaces it anyway.
        let progress: buckle_nomad::primitives::Progress = {
            let (events, module, task_id, id, kind) = (
                events.clone(),
                module.clone(),
                step.key.clone(),
                id.clone(),
                kind.clone(),
            );
            std::sync::Arc::new(move |message: &str| {
                let _ = events.try_send(ApplyEvent::StepProgress {
                    module: module.clone(),
                    task_id: task_id.clone(),
                    id: id.clone(),
                    kind: kind.clone(),
                    message: message.to_owned(),
                });
            })
        };
        let applied = unit.plan.apply(i, progress).await;
        let outcome = steps.get_mut(i);
        match applied {
            Ok(applied) => {
                if applied.changed {
                    module_changes += 1;
                }
                emit(ApplyEvent::StepFinished {
                    module: module.clone(),
                    task_id: step.key.clone(),
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
                    task_id: step.key.clone(),
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
    let mut result = ModuleOutcome {
        changes: module_changes,
        duration_ms: module_clock.elapsed().as_millis() as u64,
        steps,
        started_at: Some(module_started),
        finished_at: Some(Utc::now()),
        ..ModuleOutcome::new(&module, status)
    };
    if let Some(e) = &module_error {
        result = result.with_error(ErrorReport::of(e));
    }
    (result, module_error)
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
    EntitySpec::new(
        EntityKind::Namespace,
        "workload.yaml namespace",
        format!("name = {}\n", serde_json::Value::String(ns.to_owned())),
    )
}

/// A templater over `w`'s templates and the workload's context with its
/// vars rendered.
fn workload_templating(
    revision: &Revision,
    w: &LoadedWorkload,
    ctx: &HostContext,
) -> buckle_core::Result<(Templater, HostContext, buckle_core::selectors::Vars)> {
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
    Ok((templater, ctx, vars))
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
    let (templater, ctx, vars) = workload_templating(revision, w, ctx)?;

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

    let namespace = cfg
        .namespace
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let paths = |entry: &ResourceEntry| {
        buckle_core::fieldpath::FieldPath::parse_all(&entry.ignore).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!(
                    "The {} of workload {} has an `ignore` path that is not valid.",
                    entry.describe(),
                    cfg.name
                ),
                &["Run `buckle validate` to see every problem with the workload."],
            )
        })
    };
    let mut primitives = Vec::with_capacity(cfg.resources.len());
    for entry in &cfg.resources {
        let file = entry.file.clone().unwrap_or_default();
        let primitive = match entry.kind {
            ResourceKind::Job => {
                let mut files = IndexMap::new();
                for (var, name) in cfg.files.iter().chain(entry.files.iter()) {
                    files.insert(var.clone(), read(name.clone()).await?);
                }
                Primitive::Job(JobSpec {
                    hcl: read(file.clone()).await?,
                    file,
                    vars: vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    files,
                    namespace: cfg.namespace.clone(),
                    live_vars: entry.live_vars.clone(),
                    ignore: paths(entry)?,
                    placement_timeout: cfg.timeouts.placement,
                    rollout_timeout: cfg.timeouts.rollout,
                })
            }
            ResourceKind::Volume => Primitive::Volume(VolumeSpec {
                hcl: read(file.clone()).await?,
                file,
            }),
            ResourceKind::Policy => Primitive::Policy(PolicySpec {
                rules: read(file.clone()).await?,
                file,
                name: entry.name.clone(),
                description: cfg.description.clone(),
                attach: entry.attach.as_ref().map(|a| Attach {
                    namespace: a.namespace.clone().or_else(|| cfg.namespace.clone()),
                    job: a.job.clone(),
                    group: a.group.clone(),
                    task: a.task.clone(),
                }),
            }),
            ResourceKind::Namespace | ResourceKind::NodePool | ResourceKind::Quota => {
                let kind = match entry.kind {
                    ResourceKind::Namespace => EntityKind::Namespace,
                    ResourceKind::NodePool => EntityKind::NodePool,
                    _ => EntityKind::Quota,
                };
                Primitive::Entity(EntitySpec {
                    ignore: paths(entry)?,
                    ..EntitySpec::new(kind, file.clone(), read(file).await?)
                })
            }
            ResourceKind::Secret | ResourceKind::Config => {
                let path = entry.name.clone().unwrap_or_default();
                let items: IndexMap<String, Rendered> = if entry.kind == ResourceKind::Config {
                    config_items(&cfg.name, entry, &read, &templater, &ctx)
                        .await?
                        .into_iter()
                        .map(|(k, v)| (k, Rendered::Plain(v)))
                        .collect()
                } else {
                    let mut rendered = IndexMap::new();
                    for (key, value) in &entry.items {
                        let item_ctx = ctx.in_task("nomad.variable", &path, &[]);
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
                    rendered
                };
                // A secret and a config at one path are one Nomad variable,
                // written together where the first of them is listed.
                let shared = primitives.iter_mut().find_map(|p| match p {
                    Primitive::Variable(v) if v.namespace == namespace && v.path == path => Some(v),
                    _ => None,
                });
                if let Some(variable) = shared {
                    for (key, value) in items {
                        if variable.items.insert(key.clone(), value).is_some() {
                            return Err(buckle_core::error::user(
                                format!(
                                    "The workload {} sets the item {key} of variable {path} twice, in a secret and a config entry.",
                                    cfg.name
                                ),
                                &[
                                    "Keep each item in one entry; both entries write the same Nomad variable.",
                                ],
                            ));
                        }
                    }
                    continue;
                }
                Primitive::Variable(VariableSpec {
                    namespace: namespace.clone(),
                    path,
                    items,
                })
            }
            ResourceKind::Manifest => {
                return Err(buckle_core::error::user(
                    format!(
                        "The workload {} lists {}, which a Nomad cluster cannot apply.",
                        cfg.name,
                        entry.describe()
                    ),
                    &["Narrow the workload's `when` to `cluster.kind == \"kubernetes\"`."],
                ));
            }
        };
        primitives.push(primitive);
    }
    Ok(Workload {
        name: cfg.name.clone(),
        primitives,
        removal: nomad_removal(cfg.removal),
    })
}

/// A `config` entry's items as the executor writes them, on either scheduler:
/// the inline `items`, then each of `files` read through `read` (which
/// renders `.j2` files), rendered plain, so that a secret value is refused
/// rather than stored in clear.
pub(crate) async fn config_items<F>(
    workload: &str,
    entry: &ResourceEntry,
    read: impl Fn(String) -> F,
    templater: &Templater,
    ctx: &HostContext,
) -> buckle_core::Result<IndexMap<String, String>>
where
    F: std::future::Future<Output = buckle_core::Result<String>>,
{
    let name = entry.name.clone().unwrap_or_default();
    let item_ctx = ctx.in_task("config", &name, &[]);
    let mut out = IndexMap::new();
    for (key, value) in &entry.items {
        let rendered = templater.render_plain(value, &item_ctx).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!(
                    "The item {key} of config {name} (workload {workload}) could not be rendered."
                ),
                &["A config is stored in clear; put secret values in a `secret` entry."],
            )
        })?;
        out.insert(key.clone(), rendered);
    }
    for (key, file) in &entry.files {
        out.insert(key.clone(), read(file.clone()).await?);
    }
    Ok(out)
}

fn nomad_removal(removal: WorkloadRemoval) -> buckle_nomad::Removal {
    match removal {
        WorkloadRemoval::Stop => buckle_nomad::Removal::Stop,
        WorkloadRemoval::Purge => buckle_nomad::Removal::Purge,
        WorkloadRemoval::Keep => buckle_nomad::Removal::Keep,
    }
}

/// The plan's resource key of a cluster object: `paste/nomad.job[default/paste]`.
fn resource_key(module: &str, kind: &str, key: &str) -> ResourceKey {
    ResourceKey::new(module, kind, key)
}

/// The plan of a cluster target, from its parts.
fn finish(
    a: &Assignment,
    modules: Vec<ModulePlan>,
    removals: Vec<Removal>,
    indeterminate: Vec<Indeterminate>,
    warnings: Vec<String>,
    units: Vec<Unit>,
    steps: Vec<RemovalStep>,
) -> ClusterPlan {
    let mut plan = Plan::new(
        a.revision.id.clone(),
        &a.target,
        modules,
        removals,
        Vec::new(),
    )
    .with_indeterminate(indeterminate);
    plan.warnings = warnings;
    ClusterPlan {
        plan,
        units,
        removals: steps,
    }
}

/// `workloads` narrowed to the placement `scope` names, when it names one.
pub(crate) fn scoped<'a>(
    workloads: Vec<&'a LoadedWorkload>,
    scope: Option<&str>,
) -> Vec<&'a LoadedWorkload> {
    match scope {
        None => workloads,
        Some(s) => workloads
            .into_iter()
            .filter(|w| w.config.name == s)
            .collect(),
    }
}

/// A workload's steps (resource key, kind, plan) as a module plan; `facts`
/// are the cluster values its templates emitted ([`Step::facts`]).
fn module_plan(
    name: &str,
    needs: &[String],
    facts: Vec<String>,
    steps: impl Iterator<Item = (String, &'static str, buckle_core::plan::Planned)>,
) -> ModulePlan {
    ModulePlan {
        module: name.to_owned(),
        needs: needs.to_vec(),
        steps: steps
            .map(|(key, kind, planned)| Step {
                task: TaskRef {
                    module: name.to_owned(),
                    id: key.clone(),
                    kind: kind.to_owned(),
                    name: None,
                },
                resource: resource_key(name, kind, &key),
                planned,
                locks: Vec::new(),
                notifies: Vec::new(),
                always_runs: false,
                removal: RemovalPolicy::Cleanup,
                facts: Some(facts.clone()),
                display: None,
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
