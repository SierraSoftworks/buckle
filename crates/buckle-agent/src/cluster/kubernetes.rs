//! Kubernetes cluster targets: where the API is, which credential to use,
//! and the plan, apply and health of their workloads through
//! [`buckle_kube`].

use super::*;
use buckle_kube::object::ObjectKey as KubeKey;
use buckle_kube::primitives::{ManifestSpec, SecretSpec};
use buckle_kube::{Endpoint, Kube};

/// Where an executor reaches a Kubernetes target, and with which of the
/// host's credentials:
///
/// * `local`: the host's kubeconfig (`KUBECONFIG`, `/etc/rancher/k3s/k3s.yaml`,
///   `~/.kube/config`), its server replaced by the cluster's `api` if set;
/// * `workload`: the pod's ServiceAccount;
/// * `buckle_jwt` and `secret://`: the cluster's `api` (else the pod's or
///   kubeconfig's server), trusting the CA found there, with no host
///   credential: the bearer token is added by the executor.
pub(super) fn endpoint(spec: &TargetSpec) -> buckle_core::Result<Endpoint> {
    let api = spec.api.clone().filter(|a| !a.trim().is_empty());
    let mut endpoint = match &spec.identity {
        ClusterIdentity::Local => Endpoint::local()?.ok_or_else(|| {
            buckle_core::error::user(
                "The cluster's identity is `local`, but this executor has no kubeconfig.",
                &[
                    "Run the executor on a k3s server (it reads /etc/rancher/k3s/k3s.yaml), or set KUBECONFIG for the agent.",
                ],
            )
        })?,
        ClusterIdentity::Workload => Endpoint::in_cluster()?.ok_or_else(|| {
            buckle_core::error::user(
                "The cluster's identity is `workload`, but this executor is not running in a pod.",
                &["Use `buckle_jwt` or `local` for an executor on a host."],
            )
        })?,
        ClusterIdentity::BuckleJwt | ClusterIdentity::Secret(_) => {
            let found = Endpoint::in_cluster()
                .ok()
                .flatten()
                .or_else(|| Endpoint::local().ok().flatten())
                .map(Endpoint::without_credential);
            match (found, &api) {
                (Some(e), _) => e,
                (None, Some(api)) => Endpoint::address(api.clone()),
                (None, None) => {
                    return Err(buckle_core::error::user(
                        "The cluster declares no `api`, and this executor has no kubeconfig to find it in.",
                        &["Set the cluster's `api` (https://host:6443) in fleet.clusters."],
                    ));
                }
            }
        }
    };
    if let Some(api) = api
        && spec.identity != ClusterIdentity::Workload
    {
        endpoint.server = api.trim_end_matches('/').to_owned();
    }
    Ok(endpoint)
}

impl Executor {
    /// A Kubernetes client for `a`'s cluster carrying a credential for
    /// `purpose`.
    pub(super) async fn kube_client(
        &self,
        a: &Assignment,
        purpose: Purpose,
        rollout: Option<&str>,
    ) -> buckle_core::Result<Kube> {
        self.kube_client_with_expiry(a, purpose, rollout)
            .await
            .map(|(kube, _)| kube)
    }

    /// [`Executor::kube_client`] with the credential's expiry, when it has
    /// one (a Buckle-issued identity).
    pub(super) async fn kube_client_with_expiry(
        &self,
        a: &Assignment,
        purpose: Purpose,
        rollout: Option<&str>,
    ) -> buckle_core::Result<(Kube, Option<chrono::DateTime<Utc>>)> {
        let kube = self.connect.kubernetes(&a.target, &a.spec)?;
        match &a.spec.identity {
            ClusterIdentity::BuckleJwt => {
                let request = IdentityRequest {
                    target: a.target.clone(),
                    purpose,
                    revision: Some(a.revision.id.clone()),
                    rollout: rollout.map(str::to_owned),
                };
                let credential = self.credentials.get(&request, &Login::Kubernetes).await?;
                let expires = credential.expires_at();
                let kube = match credential {
                    Credential::Bearer { token, .. } => kube.token(token.into()),
                    Credential::Nomad { secret_id, .. } => kube.token(secret_id.into()),
                };
                Ok((kube, Some(expires)))
            }
            ClusterIdentity::Local | ClusterIdentity::Workload => Ok((kube, None)),
            ClusterIdentity::Secret(name) => match a.secrets.get(name) {
                Some(secret) => Ok((kube.token(ApiSink.credential(secret)), None)),
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

    /// [`Executor::plan`] for a Kubernetes cluster.
    pub(super) async fn plan_kubernetes(
        &self,
        a: &Assignment,
        rollout: Option<&str>,
        scope: Option<&str>,
    ) -> buckle_core::Result<ClusterPlan> {
        let kube = self.kube_client(a, Purpose::Deploy, rollout).await?;
        let ctx = a.context();
        let workloads = super::scoped(a.workloads(&ctx), scope);
        let base = buckle_kube::Context::new(
            kube.clone(),
            &a.target,
            "",
            &a.revision.id.0,
            a.spec.fingerprints.clone(),
        );

        // A workload's `namespace` the cluster lacks is created by the
        // first workload naming it; one that exists is left alone.
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let mut declared: Vec<KubeKey> = Vec::new();
        let wanted: BTreeSet<&str> = workloads
            .iter()
            .filter_map(|w| w.config.namespace.as_deref())
            .filter(|ns| !ns.is_empty() && *ns != "default")
            .collect();
        for ns in wanted {
            let path = format!("/api/v1/namespaces/{}", buckle_kube::client::seg(ns));
            match kube.get(&path).await? {
                Some(_) => declared.push(KubeKey::new("v1", "Namespace", "", ns)),
                None => {
                    missing.insert(ns.to_owned());
                }
            }
        }

        let selected: BTreeSet<String> = workloads.iter().map(|w| w.config.name.clone()).collect();
        let mut units = Vec::new();
        let mut modules = Vec::new();
        let mut warnings = Vec::new();
        let mut indeterminate = Vec::new();
        let mut undecided: BTreeSet<String> = BTreeSet::new();
        let mut earlier: Vec<KubeKey> = Vec::new();
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
            let compiled = if w.config.uses_nomad_keys() {
                Err(buckle_core::error::user(
                    format!(
                        "The workload {name} lists Nomad resources, but {} is a Kubernetes cluster.",
                        a.target
                    ),
                    &["Narrow the workload's `when` to `cluster.kind == \"nomad\"`."],
                ))
            } else {
                // The workload's renders are traced, so the fleet view marks
                // only the cluster values its templates emitted.
                compile(&a.revision, w, &ctx.clone().with_emitted(emitted.clone())).await
            };
            let planned = match compiled {
                Ok(mut workload) => {
                    if let Some(ns) = w.config.namespace.as_deref()
                        && missing.remove(ns)
                    {
                        workload.primitives.insert(
                            0,
                            buckle_kube::Primitive::Manifest(buckle_kube::primitives::namespace(
                                ns,
                            )),
                        );
                    }
                    let cx = base
                        .clone()
                        .with_namespace(w.config.namespace.as_deref())
                        .with_timeouts(w.config.timeouts.placement, w.config.timeouts.rollout);
                    let cx = buckle_kube::Context {
                        module: name.clone(),
                        ..cx
                    };
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
                        matches!(k.kind.as_str(), "Namespace" | "CustomResourceDefinition")
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
                            .map(|s| (s.key.key(), s.task_kind(), s.planned.clone())),
                    ));
                    units.push(Unit {
                        name,
                        plan: UnitPlan::Kube(cx, plan),
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

        let mut removals = Vec::new();
        let mut steps = Vec::new();
        for orphan in buckle_kube::primitives::orphans(&base, &declared).await? {
            // A placement removes only what its own workload owned.
            if undecided.contains(&orphan.module) || scope.is_some_and(|s| s != orphan.module) {
                continue;
            }
            let removal = a
                .revision
                .workload(&orphan.module)
                .map(|w| kube_removal(w.config.removal))
                .unwrap_or_default();
            let kind = if orphan.key.kind == "Secret" && orphan.key.api_version == "v1" {
                buckle_kube::primitives::SECRET_KIND
            } else {
                buckle_kube::primitives::MANIFEST_KIND
            };
            removals.push(Removal {
                resource: resource_key(&orphan.module, kind, &orphan.key.key()),
                policy: if orphan.describe(removal).is_some() {
                    RemovalPolicy::Cleanup
                } else {
                    RemovalPolicy::Leave
                },
                summary: orphan
                    .describe(removal)
                    .unwrap_or_else(|| format!("keep {} in the cluster", orphan.key)),
                caution: orphan.caution(removal),
                introduced_in: None,
            });
            let ocx = buckle_kube::Context {
                module: orphan.module.clone(),
                ..base.clone()
            };
            steps.push(RemovalStep::Kube(orphan, removal, ocx));
        }
        Ok(finish(
            a,
            modules,
            removals,
            indeterminate,
            warnings,
            units,
            steps,
        ))
    }

    /// [`Executor::health`] for a Kubernetes cluster: each controller's
    /// rollout and pods, and every other object with a status to follow.
    pub(super) async fn health_kubernetes(
        &self,
        a: &Assignment,
        scope: HealthScope,
        modules: Option<&[String]>,
        rollout: Option<&str>,
    ) -> buckle_core::Result<HealthReport> {
        let kube = self.kube_client(a, Purpose::Read, rollout).await?;
        let ctx = a.context();
        let cx = buckle_kube::Context::new(
            kube.clone(),
            &a.target,
            "",
            &a.revision.id.0,
            a.spec.fingerprints.clone(),
        );
        let mut owned: HashMap<String, Vec<KubeKey>> = HashMap::new();
        for e in buckle_kube::primitives::owned(&cx).await? {
            owned.entry(e.workload).or_default().push(e.key);
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
            let keys = owned.remove(module).unwrap_or_default();
            if w.config.of_kind(ResourceKind::Manifest).next().is_some() && keys.is_empty() {
                results.push(result(
                    "objects",
                    buckle_kube::primitives::MANIFEST_KIND,
                    true,
                    HealthStatus::fail(format!("the objects of {module} are not applied")),
                ));
            }
            for key in keys.iter().filter(|k| has_status(k)) {
                let started = tokio::time::Instant::now();
                for check in buckle_kube::health::object(&kube, key).await? {
                    let mut r = result(
                        &check.name,
                        buckle_kube::primitives::MANIFEST_KIND,
                        true,
                        check.status,
                    );
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

/// Objects whose status says something about health (and that the `view`
/// role a `read` credential carries may read).
fn has_status(key: &KubeKey) -> bool {
    buckle_kube::health::is_rollout(key)
        || matches!(
            (key.group(), key.kind.as_str()),
            ("batch", "Job") | ("", "PersistentVolumeClaim") | ("", "Pod")
        )
}

fn kube_removal(removal: WorkloadRemoval) -> buckle_kube::Removal {
    match removal {
        WorkloadRemoval::Stop => buckle_kube::Removal::Stop,
        WorkloadRemoval::Purge => buckle_kube::Removal::Purge,
        WorkloadRemoval::Keep => buckle_kube::Removal::Keep,
    }
}

/// `workload.yaml`'s resources as Kubernetes objects: manifests read (`.j2`
/// rendered with the workload's vars) and split into documents, secrets
/// rendered (with secret values, kept sensitive).
async fn compile(
    revision: &Revision,
    w: &LoadedWorkload,
    ctx: &HostContext,
) -> buckle_core::Result<buckle_kube::Workload> {
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
    let ctx = ctx.with_vars(vars);

    let read = async |file: &str| -> buckle_core::Result<String> {
        let bytes = w.source.read(Path::new(file)).await?.ok_or_else(|| {
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
                &["Manifests are YAML or JSON text."],
            )
        })?;
        if file.ends_with(".j2") {
            templater.render_plain(&text, &ctx).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("{file} of workload {} could not be rendered.", cfg.name),
                    &["Secrets reach a pod through `secret` entries (Kubernetes Secrets), never a manifest."],
                )
            })
        } else {
            Ok(text)
        }
    };

    let mut primitives = Vec::new();
    for entry in &cfg.resources {
        match entry.kind {
            ResourceKind::Manifest => {
                let file = entry.file.clone().unwrap_or_default();
                let ignore =
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
                    })?;
                let text = read(&file).await?;
                for object in buckle_kube::object::parse_manifests(&text, &file)? {
                    primitives.push(buckle_kube::Primitive::Manifest(ManifestSpec {
                        file: file.clone(),
                        object,
                        ignore: ignore.clone(),
                    }));
                }
            }
            ResourceKind::Secret => {
                let name = entry.name.clone().unwrap_or_default();
                let mut items = IndexMap::new();
                for (key, value) in &entry.items {
                    let item_ctx = ctx.in_task(buckle_kube::primitives::SECRET_KIND, &name, &[]);
                    items.insert(
                        key.clone(),
                        templater.render_sensitive(value, &item_ctx).map_err(|e| {
                            buckle_core::error::wrap_user(
                                e,
                                format!(
                                    "The item {key} of secret {name} (workload {}) could not be rendered.",
                                    cfg.name
                                ),
                                &["Check the template and that the secrets it reads are declared."],
                            )
                        })?,
                    );
                }
                primitives.push(buckle_kube::Primitive::Secret(SecretSpec {
                    namespace: String::new(),
                    name,
                    secret_type: entry.secret_type.clone(),
                    items,
                }));
            }
            ResourceKind::Config => {
                let name = entry.name.clone().unwrap_or_default();
                let read = &read;
                let items = super::config_items(
                    &cfg.name,
                    entry,
                    |file: String| async move { read(&file).await },
                    &templater,
                    &ctx,
                )
                .await?;
                primitives.push(buckle_kube::Primitive::Manifest(ManifestSpec::config_map(
                    &name, None, items,
                )));
            }
            _ => {
                return Err(buckle_core::error::user(
                    format!(
                        "The workload {} lists {}, which a Kubernetes cluster cannot apply.",
                        cfg.name,
                        entry.describe()
                    ),
                    &["Narrow the workload's `when` to `cluster.kind == \"nomad\"`."],
                ));
            }
        }
    }
    Ok(buckle_kube::Workload {
        name: cfg.name.clone(),
        primitives,
        removal: kube_removal(cfg.removal),
    })
}
