//! The executor against the in-memory Nomad: plan, apply, re-plan, removal,
//! ordering, health and credentials.

use super::*;
use crate::identity::IssuedIdentity;
use crate::transport::{Hello, RevisionSource, Session, WireSecret};
use buckle_core::secrets::Secret;
use buckle_nomad::testing::FakeNomad;
use rstest::rstest;

const TARGET: &str = "home-nomad";
const SECRET: &str = "hunter2-SENTINEL";

/// Issues `jwt:<target>:<purpose>` identities.
struct Issuing;

#[async_trait::async_trait]
impl Transport for Issuing {
    async fn connect(&self, _hello: Hello) -> buckle_core::Result<Session> {
        unimplemented!()
    }
    async fn fetch_revision(
        &self,
        _revision: &RevisionId,
        _tree_digest: Option<&str>,
    ) -> buckle_core::Result<RevisionSource> {
        unimplemented!()
    }
    async fn fetch_secrets(
        &self,
        _revision: &RevisionId,
        _names: &[String],
    ) -> buckle_core::Result<Vec<WireSecret>> {
        unimplemented!()
    }
    async fn issue_identity(
        &self,
        request: &IdentityRequest,
    ) -> buckle_core::Result<IssuedIdentity> {
        Ok(IssuedIdentity {
            token: format!("jwt:{}:{:?}", request.target, request.purpose),
            expires_at: Utc::now() + chrono::Duration::minutes(15),
            audience: request.target.clone(),
            id: "jti".into(),
        })
    }
}

struct Fake(FakeNomad);

impl Connect for Fake {
    fn nomad(&self, _target: &str, _spec: &TargetSpec) -> buckle_core::Result<Nomad> {
        Ok(self.0.client())
    }
}

fn executor(fake: &FakeNomad) -> Executor {
    Executor::new(Arc::new(Issuing), Arc::new(Fake(fake.clone())))
}

const FLEET: &str = "version: 2\nfleet:\n  clusters:\n    home-nomad: { kind: nomad }\nsecrets:\n  DB: { env: DB }\n";

fn job_spec(id: &str) -> String {
    serde_json::json!({
        "ID": id, "Type": "service",
        "TaskGroups": [{"Name": "api", "Count": 1, "Tasks": [{"Name": id, "Config": {"image": format!("{id}:1")}}]}],
    })
    .to_string()
}

/// A workload `name` with a job, plus the extra `yaml` keys.
fn workload_files(name: &str, yaml: &str) -> Vec<(String, String)> {
    vec![
        (
            format!("workloads/{name}/workload.yaml"),
            format!("name: {name}\nresources: [{name}.nomad.hcl]\n{yaml}"),
        ),
        (
            format!("workloads/{name}/files/{name}.nomad.hcl"),
            job_spec(name),
        ),
    ]
}

fn revision(id: &str, files: &[(String, String)]) -> Arc<Revision> {
    revision_with(id, files, |_| {})
}

/// [`revision`], edited after it loads.
fn revision_with(
    id: &str,
    files: &[(String, String)],
    edit: impl FnOnce(&mut Revision),
) -> Arc<Revision> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("buckle.yaml"), FLEET).unwrap();
    for (path, content) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    // The loader reads files lazily: keep the directory for the test's life.
    let path = dir.keep();
    let mut rev = buckle_core::engine::loader::load_dir(&path, RevisionId::new(id)).unwrap();
    edit(&mut rev);
    Arc::new(rev)
}

fn assignment(revision: Arc<Revision>, identity: ClusterIdentity, key: u8) -> Assignment {
    let mut secrets = SecretStore::new();
    secrets.insert(Secret::new(
        "DB",
        SECRET.into(),
        &InstallKey::from_bytes([1; 32]),
    ));
    Assignment {
        target: TARGET.into(),
        spec: Arc::new(TargetSpec {
            kind: ClusterKind::Nomad,
            api: None,
            identity,
            labels: IndexMap::new(),
            fingerprints: Arc::new(InstallKey::from_bytes([key; 32])),
        }),
        revision,
        secrets: Arc::new(secrets),
    }
}

fn paste() -> Vec<(String, String)> {
    let mut files = workload_files("paste", "");
    files[0].1 = "name: paste\nnamespace: apps\nresources:\n  - secret: nomad/jobs/paste\n    items:\n      DATABASE_URL: 'postgres://paste:{{ secrets.DB }}@db/paste'\n  - paste.nomad.hcl\n".to_owned();
    files
}

async fn apply(ex: &Executor, a: &Assignment) -> ApplyOutcome {
    apply_with_events(ex, a).await.0
}

/// Applies `a`'s plan, with the events the apply sent.
async fn apply_with_events(
    ex: &Executor,
    a: &Assignment,
) -> (ApplyOutcome, Vec<buckle_core::engine::ApplyEvent>) {
    let planned = ex.plan(a, None, None).await.unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        seen
    });
    let outcome = ex.apply(&planned, tx).await;
    (outcome, drain.await.unwrap())
}

#[tokio::test]
async fn a_workload_plans_applies_and_then_plans_as_a_no_op() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(revision("r1", &paste()), ClusterIdentity::BuckleJwt, 7);

    let planned = ex.plan(&a, None, None).await.unwrap();
    let plan = &planned.plan;
    assert_eq!(plan.host, TARGET);
    assert_eq!(plan.modules.len(), 1);
    let kinds: Vec<&str> = plan.modules[0]
        .steps
        .iter()
        .filter(|s| s.planned.is_change())
        .map(|s| s.resource.kind.as_str())
        .collect();
    for kind in ["nomad.namespace", "nomad.variable", "nomad.job"] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    let json = serde_json::to_string(plan).unwrap();
    assert!(!json.contains(SECRET), "the secret leaked into the plan");

    let (outcome, events) = apply_with_events(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    // The job's deployment is followed step by step.
    assert!(
        events.iter().any(|e| matches!(
            e,
            buckle_core::engine::ApplyEvent::StepProgress { module, kind, message, .. }
                if module == "paste" && kind == "nomad.job" && message.contains("healthy")
        )),
        "{events:?}"
    );
    assert!(fake.entity("namespace", "apps").is_some());
    assert!(fake.variable("apps", "nomad/jobs/paste").is_some());

    let again = ex.plan(&a, None, None).await.unwrap().plan;
    assert_eq!(again.changes(), 0, "{again:#?}");
    assert!(again.removals.is_empty(), "{:?}", again.removals);
    // Nomad reports the untouched allocation as `ignore`: not a warning.
    assert!(again.warnings.is_empty(), "{:?}", again.warnings);
}

#[tokio::test]
async fn buckle_identities_log_in_per_purpose_and_authenticate_requests() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(revision("r1", &paste()), ClusterIdentity::BuckleJwt, 7);
    apply(&ex, &a).await;

    assert_eq!(fake.logins(), vec![format!("jwt:{TARGET}:Deploy")]);
    let tokens = fake.tokens();
    assert!(
        tokens
            .iter()
            .all(|t| t.is_empty() || t == &format!("acl:jwt:{TARGET}:Deploy")),
        "{tokens:?}"
    );
    assert!(tokens.iter().any(|t| !t.is_empty()));
}

#[tokio::test]
async fn plans_deploy_and_health_checks_read() {
    // Nomad's job plan needs submit-job, so only health checks stay read-only.
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(revision("r1", &paste()), ClusterIdentity::BuckleJwt, 7);
    ex.plan(&a, None, None).await.unwrap();
    assert_eq!(fake.logins(), vec![format!("jwt:{TARGET}:Deploy")]);
    ex.health(&a, HealthScope::Bake, None, None).await.unwrap();
    assert!(
        fake.logins().contains(&format!("jwt:{TARGET}:Read")),
        "{:?}",
        fake.logins()
    );
}

#[tokio::test]
async fn a_secret_identity_sends_the_secret_as_the_token() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(
        revision("r1", &paste()),
        ClusterIdentity::Secret("DB".into()),
        7,
    );
    ex.plan(&a, None, None).await.unwrap();
    assert!(fake.logins().is_empty());
    assert!(fake.tokens().iter().all(|t| t == SECRET));
}

#[rstest]
#[case::same_key(7, true)]
#[case::other_key(8, false)]
#[tokio::test]
async fn plan_digests_depend_on_the_targets_key_not_the_executor(
    #[case] other: u8,
    #[case] same: bool,
) {
    let fake = FakeNomad::new();
    let rev = revision("r1", &paste());
    let one = executor(&fake)
        .plan(
            &assignment(rev.clone(), ClusterIdentity::BuckleJwt, 7),
            None,
            None,
        )
        .await
        .unwrap();
    let two = executor(&fake)
        .plan(
            &assignment(rev, ClusterIdentity::BuckleJwt, other),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(one.plan.digest == two.plan.digest, same);
}

#[tokio::test]
async fn a_removed_workload_is_planned_as_a_removal_and_stopped() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let mut both = workload_files("paste", "");
    both.extend(workload_files("atuin", ""));
    apply(
        &ex,
        &assignment(revision("r1", &both), ClusterIdentity::Local, 7),
    )
    .await;

    let a = assignment(
        revision("r2", &workload_files("paste", "")),
        ClusterIdentity::Local,
        7,
    );
    let plan = ex.plan(&a, None, None).await.unwrap().plan;
    let removed: Vec<String> = plan
        .removals
        .iter()
        .map(|r| r.resource.to_string())
        .collect();
    assert_eq!(removed, vec!["atuin/nomad.job[default/atuin]"]);
    assert!(plan.removals[0].summary.contains("stop"));

    let outcome = apply(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    assert_eq!(fake.job("default", "atuin").unwrap()["Stop"], true);
}

#[tokio::test]
async fn a_workload_that_cannot_be_planned_is_indeterminate_and_keeps_its_objects() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    apply(
        &ex,
        &assignment(revision("r1", &paste()), ClusterIdentity::Local, 7),
    )
    .await;

    // The secret its variable reads did not reach the executor.
    let mut a = assignment(revision("r2", &paste()), ClusterIdentity::Local, 7);
    a.secrets = Arc::new(SecretStore::new());
    let planned = ex.plan(&a, None, None).await.unwrap();
    assert_eq!(planned.plan.indeterminate.len(), 1);
    assert_eq!(planned.plan.indeterminate[0].module, "paste");
    assert!(
        planned.plan.removals.is_empty(),
        "{:?}",
        planned.plan.removals
    );

    let (tx, _rx) = mpsc::channel(256);
    let outcome = ex.apply(&planned, tx).await;
    assert!(!outcome.succeeded);
    assert_eq!(fake.job("apps", "paste").unwrap()["Stop"], false);
}

#[tokio::test]
async fn workloads_apply_after_the_workloads_they_need() {
    let fake = FakeNomad::new();
    let mut files = workload_files("a-web", "needs: [z-db, 'module:nomad']\n");
    files.extend(workload_files("z-db", ""));
    files.push((
        "modules/nomad/module.yaml".into(),
        "name: nomad\ntasks: []\n".into(),
    ));
    let a = assignment(revision("r1", &files), ClusterIdentity::Local, 7);
    let planned = executor(&fake).plan(&a, None, None).await.unwrap();
    let order: Vec<&str> = planned
        .plan
        .modules
        .iter()
        .map(|m| m.module.as_str())
        .collect();
    assert_eq!(order, vec!["z-db", "a-web"]);
}

/// While `first`'s deployment is held, whether `second` gets submitted.
async fn submitted_alongside(
    files: Vec<(String, String)>,
    first: (&str, &str),
    second: (&str, &str),
) -> (bool, ApplyOutcome) {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(revision("r1", &files), ClusterIdentity::Local, 7);
    let planned = ex.plan(&a, None, None).await.unwrap();
    for (ns, id) in [first, second] {
        fake.hold(ns, id);
    }
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let watch = async {
        while fake.job(first.0, first.1).is_none() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        // Long enough for several deployment polls.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let alongside = fake.job(second.0, second.1).is_some();
        for (ns, id) in [first, second] {
            fake.release(ns, id);
        }
        alongside
    };
    let (alongside, outcome) = tokio::join!(watch, ex.apply(&planned, tx));
    drain.await.unwrap();
    (alongside, outcome)
}

#[rstest]
#[case::independent("", "", "default", true)]
#[case::needed("needs: [z-db]\n", "", "default", false)]
// a-web creates the namespace z-db's job lands in.
#[case::namespace_created_earlier("namespace: apps\n", "namespace: apps\n", "apps", false)]
#[tokio::test(start_paused = true)]
async fn workloads_deploy_concurrently_unless_one_waits_for_another(
    #[case] web: &str,
    #[case] db: &str,
    #[case] ns: &str,
    #[case] concurrent: bool,
) {
    let mut files = workload_files("a-web", web);
    files.extend(workload_files("z-db", db));
    let (first, second) = if web.contains("needs") {
        ((ns, "z-db"), (ns, "a-web"))
    } else {
        ((ns, "a-web"), (ns, "z-db"))
    };
    let (alongside, outcome) = submitted_alongside(files, first, second).await;
    assert_eq!(alongside, concurrent);
    assert!(outcome.succeeded, "{:?}", outcome.error);
    assert!(
        outcome
            .module_results
            .iter()
            .all(|m| m.status == ModuleStatus::Applied)
    );
}

#[tokio::test]
async fn concurrent_workloads_share_the_index_variable() {
    // Each workload's variable writes an index entry.
    let fake = FakeNomad::new();
    let files: Vec<(String, String)> = (0..12)
        .flat_map(|i| {
            workload_files(
                &format!("w{i}"),
                &format!("variables:\n  nomad/jobs/w{i}:\n    A: '{i}'\n"),
            )
        })
        .collect();
    let a = assignment(revision("r1", &files), ClusterIdentity::Local, 7);
    let outcome = apply(&executor(&fake), &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    for i in 0..12 {
        assert!(
            fake.variable("default", &format!("nomad/jobs/w{i}"))
                .is_some()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_workload_needing_a_failed_one_does_not_start() {
    let fake = FakeNomad::new();
    fake.deployment_outcome("failed");
    let mut files = workload_files("a-web", "needs: [z-db]\n");
    files.extend(workload_files("z-db", ""));
    let outcome = apply(
        &executor(&fake),
        &assignment(revision("r1", &files), ClusterIdentity::Local, 7),
    )
    .await;
    assert!(!outcome.succeeded);
    assert_eq!(outcome.failed_module.as_deref(), Some("z-db"));
    assert_eq!(outcome.not_started, vec!["a-web"]);
    assert!(fake.job("default", "a-web").is_none());
}

#[tokio::test]
async fn resources_apply_in_list_order_and_every_job_is_health_checked() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let files = vec![
        (
            "workloads/shop/workload.yaml".to_owned(),
            "name: shop\nresources:\n  - shop.volume.hcl\n  - shop-api.nomad.hcl\n  - { file: shop-worker.job.hcl, ignore: ['TaskGroups[*].Count'] }\n".to_owned(),
        ),
        (
            "workloads/shop/files/shop.volume.hcl".to_owned(),
            "name = \"shop\"\ntype = \"host\"\nplugin_id = \"mkdir\"\n".to_owned(),
        ),
        (
            "workloads/shop/files/shop-api.nomad.hcl".to_owned(),
            job_spec("shop-api"),
        ),
        (
            "workloads/shop/files/shop-worker.job.hcl".to_owned(),
            job_spec("shop-worker"),
        ),
    ];
    let a = assignment(revision("r1", &files), ClusterIdentity::Local, 7);
    let planned = ex.plan(&a, None, None).await.unwrap();
    let steps: Vec<String> = planned.plan.modules[0]
        .steps
        .iter()
        .map(|s| s.resource.to_string())
        .collect();
    assert_eq!(
        steps,
        vec![
            "shop/nomad.volume[default/shop]",
            "shop/nomad.job[default/shop-api]",
            "shop/nomad.job[default/shop-worker]"
        ]
    );
    let outcome = apply(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);

    // Both jobs run, so both report; a count another tool sets is not a change.
    let report = ex.health(&a, HealthScope::Bake, None, None).await.unwrap();
    assert_eq!(
        report
            .results
            .iter()
            .filter(|r| r.module == "shop" && r.kind == "nomad.job")
            .count(),
        2 * report
            .results
            .iter()
            .filter(|r| r.module == "shop" && r.kind == "nomad.job")
            .map(|r| &r.check)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        "{report:?}"
    );
    fake.edit_job("default", "shop-worker", |j| {
        j["TaskGroups"][0]["Count"] = 3.into()
    });
    assert_eq!(ex.plan(&a, None, None).await.unwrap().plan.changes(), 0);
}

#[tokio::test]
async fn a_config_writes_its_files_as_plain_items_of_the_variable_a_secret_shares() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let files = vec![
        (
            "workloads/app/workload.yaml".to_owned(),
            "name: app\nvars: { level: debug }\nresources:\n  - { secret: nomad/jobs/app, items: { token: '{{ secrets.DB }}' } }\n  - { config: nomad/jobs/app, files: [config.yaml, extra.toml.j2], items: { mode: fast } }\n  - app.nomad.hcl\n".to_owned(),
        ),
        (
            "workloads/app/files/config.yaml".to_owned(),
            "port: 8080\ntemplate: '{{ not rendered }}'\n".to_owned(),
        ),
        (
            "workloads/app/files/extra.toml.j2".to_owned(),
            "level = \"{{ vars.level }}\"\n".to_owned(),
        ),
        ("workloads/app/files/app.nomad.hcl".to_owned(), job_spec("app")),
    ];
    let a = assignment(revision("r1", &files), ClusterIdentity::Local, 7);
    let planned = ex.plan(&a, None, None).await.unwrap();
    let steps: Vec<String> = planned.plan.modules[0]
        .steps
        .iter()
        .map(|s| s.resource.to_string())
        .collect();
    // One variable for both entries, where the first of them is listed.
    assert_eq!(
        steps,
        vec![
            "app/nomad.variable[default/nomad/jobs/app]",
            "app/nomad.job[default/app]"
        ]
    );
    let json = serde_json::to_string(&planned.plan).unwrap();
    assert!(
        json.contains("port: 8080"),
        "config values are shown in the plan"
    );
    assert!(!json.contains(SECRET), "the secret leaked into the plan");

    let outcome = apply(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    let items = fake.variable("default", "nomad/jobs/app").unwrap();
    assert_eq!(
        items["config.yaml"],
        "port: 8080\ntemplate: '{{ not rendered }}'\n"
    );
    assert_eq!(items["extra.toml"], "level = \"debug\"\n");
    assert_eq!(items["mode"], "fast");
    assert_eq!(items["token"], SECRET);
    assert_eq!(ex.plan(&a, None, None).await.unwrap().plan.changes(), 0);
}

#[tokio::test]
async fn health_checks_the_jobs_of_the_selected_workloads() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    apply(
        &ex,
        &assignment(
            revision("r1", &workload_files("paste", "")),
            ClusterIdentity::Local,
            7,
        ),
    )
    .await;

    let mut files = workload_files(
        "paste",
        "health:\n  - name: api\n    http: { url: 'http://paste.example/' }\n",
    );
    files.extend(workload_files("atuin", ""));
    let a = assignment(revision("r2", &files), ClusterIdentity::Local, 7);
    let report = ex.health(&a, HealthScope::Bake, None, None).await.unwrap();
    let by = |module: &str, check: &str| {
        report
            .results
            .iter()
            .find(|r| r.module == module && r.check == check)
            .unwrap_or_else(|| panic!("no {module}/{check} in {report:?}"))
            .status
            .clone()
    };
    assert!(by("paste", "job").is_pass());
    assert!(matches!(by("paste", "api"), HealthStatus::Unknown { .. }));
    assert!(matches!(by("atuin", "job"), HealthStatus::Fail { .. }));

    let only = ex
        .health(&a, HealthScope::Manual, Some(&["paste".to_owned()]), None)
        .await
        .unwrap();
    assert!(only.results.iter().all(|r| r.module == "paste"));
}

struct KubeFake(buckle_kube::testing::FakeKube);

impl Connect for KubeFake {
    fn nomad(&self, _target: &str, _spec: &TargetSpec) -> buckle_core::Result<Nomad> {
        unreachable!("a Kubernetes target never asks for Nomad")
    }
    fn kubernetes(
        &self,
        _target: &str,
        _spec: &TargetSpec,
    ) -> buckle_core::Result<buckle_kube::Kube> {
        Ok(self.0.client())
    }
}

fn kube_assignment(revision: Arc<Revision>, identity: ClusterIdentity) -> Assignment {
    let mut a = assignment(revision, identity, 7);
    let mut spec = (*a.spec).clone();
    spec.kind = ClusterKind::Kubernetes;
    a.spec = Arc::new(spec);
    a
}

const GRAFANA: &str = r#"apiVersion: apps/v1
kind: Deployment
metadata: { name: grafana }
spec:
  selector: { matchLabels: { app: grafana } }
  template:
    metadata: { labels: { app: grafana } }
    spec:
      containers:
        - { name: grafana, image: "grafana:{{ vars.version }}" }
---
apiVersion: v1
kind: Service
metadata: { name: grafana }
spec:
  selector: { app: grafana }
  ports: [{ name: http, port: 3000 }]
"#;

/// A Kubernetes workload: a namespace, a secret, a config and a two-object
/// manifest rendered with `version`.
fn grafana(version: &str) -> Vec<(String, String)> {
    vec![
        (
            "workloads/grafana/workload.yaml".into(),
            format!(
                "name: grafana\nnamespace: monitoring\nvars: {{ version: \"{version}\" }}\nresources:\n  - secret: grafana\n    items:\n      GF_DATABASE_PASSWORD: '{{{{ secrets.DB }}}}'\n  - {{ config: grafana, files: [grafana.ini] }}\n  - grafana.yaml.j2\n"
            ),
        ),
        (
            "workloads/grafana/files/grafana.yaml.j2".into(),
            GRAFANA.into(),
        ),
        (
            "workloads/grafana/files/grafana.ini".into(),
            "[server]\nhttp_port = 3000\n".into(),
        ),
    ]
}

#[tokio::test]
async fn a_kubernetes_workload_plans_applies_and_then_plans_as_a_no_op() {
    let fake = buckle_kube::testing::FakeKube::new();
    let ex = Executor::new(Arc::new(Issuing), Arc::new(KubeFake(fake.clone())));
    let a = kube_assignment(
        revision("r1", &grafana("11.3.1")),
        ClusterIdentity::BuckleJwt,
    );

    let planned = ex.plan(&a, None, None).await.unwrap();
    let resources: Vec<String> = planned.plan.modules[0]
        .steps
        .iter()
        .map(|s| s.resource.to_string())
        .collect();
    assert_eq!(
        resources,
        vec![
            "grafana/kubernetes.manifest[Namespace/monitoring]",
            "grafana/kubernetes.secret[monitoring/Secret/grafana]",
            "grafana/kubernetes.manifest[monitoring/ConfigMap/grafana]",
            "grafana/kubernetes.manifest[monitoring/Deployment/grafana]",
            "grafana/kubernetes.manifest[monitoring/Service/grafana]",
        ]
    );
    let json = serde_json::to_string(&planned.plan).unwrap();
    assert!(!json.contains(SECRET), "the secret leaked into the plan");

    let outcome = apply(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    let d = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert_eq!(
        d["spec"]["template"]["spec"]["containers"][0]["image"],
        "grafana:11.3.1"
    );
    let cm = fake
        .object("v1", "ConfigMap", "monitoring", "grafana")
        .unwrap();
    assert!(
        cm["data"]["grafana.ini"]
            .as_str()
            .unwrap()
            .contains("http_port")
    );
    let auth = fake.authorizations();
    assert!(
        auth.iter()
            .all(|t| t == &format!("Bearer jwt:{TARGET}:Deploy")),
        "{auth:?}"
    );

    let again = ex.plan(&a, None, None).await.unwrap().plan;
    assert_eq!(again.changes(), 0, "{again:#?}");
    assert!(again.removals.is_empty(), "{:?}", again.removals);

    let next = kube_assignment(
        revision("r2", &grafana("11.4.0")),
        ClusterIdentity::BuckleJwt,
    );
    let changed = ex.plan(&next, None, None).await.unwrap().plan;
    assert_eq!(changed.changes(), 1, "only the Deployment's image changes");

    let health = ex.health(&a, HealthScope::Bake, None, None).await.unwrap();
    assert!(health.healthy(&[]), "{:?}", health.results);
    assert!(
        health
            .results
            .iter()
            .any(|r| r.check.ends_with("Deployment/grafana pods"))
    );
    assert!(fake.authorizations().iter().any(|t| t.ends_with(":Read")));
}

#[tokio::test]
async fn a_removed_kubernetes_workload_is_planned_as_a_removal() {
    let fake = buckle_kube::testing::FakeKube::new();
    let ex = Executor::new(Arc::new(Issuing), Arc::new(KubeFake(fake.clone())));
    apply(
        &ex,
        &kube_assignment(revision("r1", &grafana("1")), ClusterIdentity::Local),
    )
    .await;

    let empty = kube_assignment(revision("r2", &[]), ClusterIdentity::Local);
    let plan = ex.plan(&empty, None, None).await.unwrap();
    let summaries: Vec<&str> = plan
        .plan
        .removals
        .iter()
        .map(|r| r.summary.as_str())
        .collect();
    assert!(
        summaries.contains(&"delete Deployment monitoring/grafana"),
        "{summaries:?}"
    );
    let outcome = ex.apply(&plan, mpsc::channel(256).0).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    assert!(
        fake.object("apps/v1", "Deployment", "monitoring", "grafana")
            .is_none()
    );
    assert!(
        fake.object("v1", "Secret", "monitoring", "grafana")
            .is_some(),
        "data stays"
    );
    assert!(
        fake.authorizations().iter().all(String::is_empty),
        "local uses the host's own credential"
    );
}

#[tokio::test]
async fn a_nomad_workload_on_a_kubernetes_cluster_is_indeterminate() {
    let fake = buckle_kube::testing::FakeKube::new();
    let ex = Executor::new(Arc::new(Issuing), Arc::new(KubeFake(fake)));
    let plan = ex
        .plan(
            &kube_assignment(revision("r1", &paste()), ClusterIdentity::Local),
            None,
            None,
        )
        .await
        .unwrap()
        .plan;
    assert_eq!(plan.indeterminate.len(), 1);
    assert!(
        plan.indeterminate[0].reason.contains("Nomad resources"),
        "{}",
        plan.indeterminate[0].reason
    );
}

#[test]
fn assignments_keep_a_few_revisions_per_target() {
    let mut held = Assignments::default();
    for i in 0..(Assignments::KEEP + 2) {
        held.insert(assignment(
            revision(&format!("r{i}"), &[]),
            ClusterIdentity::Local,
            7,
        ));
    }
    assert!(held.get(TARGET, &RevisionId::new("r0")).is_none());
    let last = RevisionId::new(format!("r{}", Assignments::KEEP + 1));
    assert!(held.get(TARGET, &last).is_some());
    assert_eq!(held.revisions().count(), Assignments::KEEP);
}

// ------------------------------------------------------------ actions

async fn run_action(ex: &Executor, a: &Assignment, workload: &str, action: &str) -> ApplyOutcome {
    run_action_on(ex, a, workload, action, None).await.0
}

/// Runs an action where `instances` says, with the events it sent.
async fn run_action_on(
    ex: &Executor,
    a: &Assignment,
    workload: &str,
    action: &str,
    instances: Option<Instances>,
) -> (ApplyOutcome, Vec<ApplyEvent>) {
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        seen
    });
    let outcome = ex.run_action(a, workload, action, instances, tx).await;
    (outcome, drain.await.unwrap())
}

fn with_backfill() -> Vec<(String, String)> {
    let mut files = workload_files(
        "paste",
        "vars: { task: backfill }\nactions:\n  backfill: { dispatch: { job: paste, meta: { task: '{{ vars.task }}' }, payload: hello }, confirm: true }\n",
    );
    files[0].1 = files[0]
        .1
        .replace("name: paste\n", "name: paste\nnamespace: apps\n");
    files
}

#[rstest]
#[case::restart("restart", "POST", "/v1/client/allocation/alloc-1/restart")]
#[case::stop("stop", "DELETE", "/v1/job/paste")]
#[case::dispatch("backfill", "POST", "/v1/job/paste/dispatch")]
#[tokio::test]
async fn workload_actions_run_on_the_workloads_jobs(
    #[case] action: &str,
    #[case] method: &str,
    #[case] path: &str,
) {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(
        revision("r1", &with_backfill()),
        ClusterIdentity::BuckleJwt,
        7,
    );
    assert!(apply(&ex, &a).await.succeeded);

    let outcome = run_action(&ex, &a, "paste", action).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    assert_eq!(outcome.changes, 1);
    assert!(
        fake.requests()
            .iter()
            .any(|(m, p)| m == method && p.starts_with(path)),
        "{:?}",
        fake.requests()
    );
}

#[rstest]
#[case::unknown_workload("nope", "restart", "not selected")]
#[case::unknown_action("paste", "vacuum", "declares no action")]
#[tokio::test]
async fn workload_actions_that_cannot_run_fail_without_touching_the_cluster(
    #[case] workload: &str,
    #[case] action: &str,
    #[case] error: &str,
) {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(
        revision("r1", &with_backfill()),
        ClusterIdentity::BuckleJwt,
        7,
    );
    assert!(apply(&ex, &a).await.succeeded);
    let before = fake.requests().len();

    let outcome = run_action(&ex, &a, workload, action).await;
    assert!(!outcome.succeeded);
    assert!(
        outcome.error.as_deref().unwrap_or_default().contains(error),
        "{:?}",
        outcome.error
    );
    assert!(
        fake.requests()[before..].iter().all(|(m, _)| m == "GET"),
        "{:?}",
        &fake.requests()[before..]
    );
}

#[tokio::test]
async fn an_implicit_action_needs_a_registered_job() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(
        revision("r1", &with_backfill()),
        ClusterIdentity::BuckleJwt,
        7,
    );
    let outcome = run_action(&ex, &a, "paste", "restart").await;
    assert!(!outcome.succeeded);
    assert!(
        outcome
            .error
            .unwrap_or_default()
            .contains("no job registered")
    );
}

/// `paste`, whose job offers `stats` on its `paste` task when `offered`
/// (the API form the fake parses), with the task action its HCL would
/// declare: `choice` where its group may run several allocations.
fn with_task_action(offered: bool, choice: bool) -> Assignment {
    let mut files = workload_files("paste", "");
    let actions = if offered {
        serde_json::json!([{"Name": "stats", "Command": "/stats"}])
    } else {
        serde_json::json!([])
    };
    files[1].1 = serde_json::json!({
        "ID": "paste", "Type": "service",
        "TaskGroups": [{"Name": "api", "Count": 2, "Tasks": [{"Name": "paste", "Config": {"image": "paste:1"}, "Actions": actions}]}],
    })
    .to_string();
    let rev = revision_with("r1", &files, |rev| {
        rev.workloads[0].task_actions = vec![TaskAction {
            name: "stats".into(),
            job: "paste".into(),
            group: "api".into(),
            task: "paste".into(),
            action: "stats".into(),
            command: Some("/stats".into()),
            instances: Instances::Any,
            choice,
        }];
    });
    assignment(rev, ClusterIdentity::BuckleJwt, 7)
}

#[rstest]
#[case::default(None, true, 1)]
#[case::any(Some(Instances::Any), true, 1)]
#[case::all(Some(Instances::All), true, 2)]
#[case::no_choice(Some(Instances::All), false, 1)]
#[tokio::test]
async fn task_actions_run_in_one_or_every_allocation(
    #[case] instances: Option<Instances>,
    #[case] choice: bool,
    #[case] runs: usize,
) {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = with_task_action(true, choice);
    assert!(apply(&ex, &a).await.succeeded);
    fake.allocations("default", "paste", 2);

    let (outcome, events) = run_action_on(&ex, &a, "paste", "stats", instances).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    let steps = &outcome.module_results[0].steps;
    assert_eq!(steps.len(), runs);
    for (i, step) in steps.iter().enumerate() {
        let alloc = format!("alloc-{}", i + 1);
        assert_eq!(step.kind, "nomad.action");
        assert!(
            step.output.as_deref().unwrap_or_default().contains(&alloc),
            "{step:?}"
        );
    }
    let sockets = fake
        .requests()
        .into_iter()
        .filter(|(m, _)| m == "WS")
        .count();
    assert_eq!(sockets, runs);
    assert!(
        events.iter().any(|e| matches!(e, ApplyEvent::StepProgress { message, .. } if message.starts_with("ran stats"))),
        "each line shows while it runs: {events:?}"
    );
}

#[tokio::test]
async fn a_task_action_that_exits_non_zero_fails_with_its_output_scrubbed() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = with_task_action(true, true);
    assert!(apply(&ex, &a).await.succeeded);
    fake.task_action("stats", &format!("connecting with {SECRET}\nrefused\n"), 2);

    let outcome = run_action(&ex, &a, "paste", "stats").await;
    assert!(!outcome.succeeded);
    assert!(outcome.error.unwrap_or_default().contains("exited 2"));
    let output = outcome.module_results[0].steps[0]
        .output
        .clone()
        .unwrap_or_default();
    assert!(
        output.contains("refused") && !output.contains(SECRET),
        "{output}"
    );
}

#[tokio::test]
async fn a_task_action_the_registered_job_lacks_does_not_run() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = with_task_action(false, true);
    assert!(apply(&ex, &a).await.succeeded);

    let outcome = run_action(&ex, &a, "paste", "stats").await;
    assert!(!outcome.succeeded);
    assert!(
        outcome
            .error
            .unwrap_or_default()
            .contains("no action stats"),
    );
    assert!(fake.requests().iter().all(|(m, _)| m != "WS"));
}

/// A plan scoped to one workload placement plans that workload alone and
/// removes only what it owned: another workload's objects are not its
/// orphans, and a workload the revision dropped plans as its removal only.
#[tokio::test]
async fn a_scoped_plan_covers_one_workload_and_its_own_orphans() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let mut both = workload_files("paste", "");
    both.extend(workload_files("grafana", ""));
    let a = assignment(revision("r1", &both), ClusterIdentity::BuckleJwt, 7);
    assert!(apply(&ex, &a).await.succeeded);

    let next = assignment(
        revision("r2", &workload_files("grafana", "")),
        ClusterIdentity::BuckleJwt,
        7,
    );
    let grafana = ex.plan(&next, None, Some("grafana")).await.unwrap().plan;
    assert_eq!(
        grafana
            .modules
            .iter()
            .map(|m| m.module.as_str())
            .collect::<Vec<_>>(),
        ["grafana"]
    );
    assert!(
        grafana.removals.is_empty(),
        "paste's objects are not grafana's orphans: {:?}",
        grafana.removals
    );

    let paste = ex.plan(&next, None, Some("paste")).await.unwrap().plan;
    assert!(paste.modules.is_empty(), "{:?}", paste.modules);
    assert!(!paste.removals.is_empty());
    assert!(
        paste.removals.iter().all(|r| r.resource.module == "paste"),
        "{:?}",
        paste.removals
    );

    let whole = ex.plan(&next, None, None).await.unwrap().plan;
    assert_eq!(whole.modules.len(), 1);
    assert_eq!(whole.removals.len(), paste.removals.len());
}
