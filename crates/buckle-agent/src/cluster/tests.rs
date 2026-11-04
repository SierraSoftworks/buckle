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
            format!("name: {name}\njob: {name}.nomad.hcl\n{yaml}"),
        ),
        (
            format!("workloads/{name}/files/{name}.nomad.hcl"),
            job_spec(name),
        ),
    ]
}

fn revision(id: &str, files: &[(String, String)]) -> Arc<Revision> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("buckle.yaml"), FLEET).unwrap();
    for (path, content) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    // The loader reads files lazily: keep the directory for the test's life.
    let path = dir.keep();
    Arc::new(buckle_core::engine::loader::load_dir(&path, RevisionId::new(id)).unwrap())
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
    workload_files(
        "paste",
        "namespace: apps\nvariables:\n  nomad/jobs/paste:\n    DATABASE_URL: 'postgres://paste:{{ secrets.DB }}@db/paste'\n",
    )
}

async fn apply(ex: &Executor, a: &Assignment) -> ApplyOutcome {
    let planned = ex.plan(a, Purpose::Deploy, None).await.unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = ex.apply(&planned, tx).await;
    drain.await.unwrap();
    outcome
}

#[tokio::test]
async fn a_workload_plans_applies_and_then_plans_as_a_no_op() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(revision("r1", &paste()), ClusterIdentity::BuckleJwt, 7);

    let planned = ex.plan(&a, Purpose::Read, None).await.unwrap();
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

    let outcome = apply(&ex, &a).await;
    assert!(outcome.succeeded, "{:?}", outcome.error);
    assert!(fake.entity("namespace", "apps").is_some());
    assert!(fake.variable("apps", "nomad/jobs/paste").is_some());

    let again = ex.plan(&a, Purpose::Read, None).await.unwrap().plan;
    assert_eq!(again.changes(), 0, "{again:#?}");
    assert!(again.removals.is_empty(), "{:?}", again.removals);
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
async fn a_secret_identity_sends_the_secret_as_the_token() {
    let fake = FakeNomad::new();
    let ex = executor(&fake);
    let a = assignment(
        revision("r1", &paste()),
        ClusterIdentity::Secret("DB".into()),
        7,
    );
    ex.plan(&a, Purpose::Read, None).await.unwrap();
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
            Purpose::Read,
            None,
        )
        .await
        .unwrap();
    let two = executor(&fake)
        .plan(
            &assignment(rev, ClusterIdentity::BuckleJwt, other),
            Purpose::Read,
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
    let plan = ex.plan(&a, Purpose::Read, None).await.unwrap().plan;
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
    let planned = ex.plan(&a, Purpose::Read, None).await.unwrap();
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
    let planned = executor(&fake).plan(&a, Purpose::Read, None).await.unwrap();
    let order: Vec<&str> = planned
        .plan
        .modules
        .iter()
        .map(|m| m.module.as_str())
        .collect();
    assert_eq!(order, vec!["z-db", "a-web"]);
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

#[tokio::test]
async fn kubernetes_clusters_are_refused_with_a_reason() {
    let fake = FakeNomad::new();
    let mut a = assignment(revision("r1", &paste()), ClusterIdentity::Local, 7);
    let mut spec = (*a.spec).clone();
    spec.kind = ClusterKind::Kubernetes;
    a.spec = Arc::new(spec);
    let err = executor(&fake)
        .plan(&a, Purpose::Read, None)
        .await
        .unwrap_err();
    assert!(
        err.description().contains("Nomad clusters only"),
        "{}",
        err.description()
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
