//! Workload scenarios against the in-memory Nomad: plan, apply, re-plan,
//! drift between plan and apply, ownership, orphans and removal.

use crate::primitives::*;
use crate::testing::FakeNomad;
use crate::{Removal, Workload, health, index, stamps};
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::{InstallKey, Rendered, Secret, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity};
use buckle_core::template::Templater;
use pretty_assertions::assert_eq;
use std::sync::{Arc, Mutex};

const SECRET: &str = "hunter2-SENTINEL";

fn cx(fake: &FakeNomad, revision: &str) -> Context {
    Context::new(
        fake.client(),
        "home-nomad",
        "atuin",
        revision,
        Arc::new(InstallKey::from_bytes([9; 32])),
    )
}

/// Renders `template` with the sentinel secret available as `secrets.db`.
fn render(template: &str) -> Rendered {
    let mut store = SecretStore::new();
    store.insert(Secret::new(
        "db",
        SECRET.into(),
        &InstallKey::from_bytes([1; 32]),
    ));
    let host = HostContext::new(HostIdentity {
        id: "home-nomad".into(),
        ..Default::default()
    })
    .with_secrets(Arc::new(store));
    Templater::new().render_sensitive(template, &host).unwrap()
}

fn job(spec: serde_json::Value) -> JobSpec {
    JobSpec {
        file: "atuin.nomad.hcl".into(),
        hcl: spec.to_string(),
        ..Default::default()
    }
}

fn atuin_job(image: &str) -> JobSpec {
    job(serde_json::json!({
        "ID": "atuin", "Type": "service",
        "TaskGroups": [{"Name": "api", "Count": 1, "Tasks": [{"Name": "atuin", "Config": {"image": image}}]}],
    }))
}

fn variables(items: &[(&str, Rendered)]) -> VariableSpec {
    VariableSpec {
        namespace: "default".into(),
        path: "nomad/jobs/atuin".into(),
        items: items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    }
}

fn workload(job: Option<JobSpec>) -> Workload {
    Workload::new("atuin", job.into_iter().map(Primitive::Job).collect())
}

/// The `atuin` workload with `primitives` in that order.
fn with(primitives: Vec<Primitive>) -> Workload {
    Workload::new("atuin", primitives)
}

fn volume() -> Primitive {
    Primitive::Volume(VolumeSpec {
        file: "atuin.volume.hcl".into(),
        hcl: "name = \"atuin\"\ntype = \"host\"\nplugin_id = \"mkdir\"\n".into(),
    })
}

#[track_caller]
fn assert_no_secret(text: &str) {
    assert!(!text.contains(SECRET), "the secret leaked into: {text}");
}

#[tokio::test]
async fn a_job_registers_follows_its_deployment_and_then_plans_as_a_no_op() {
    let fake = FakeNomad::new();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let cx =
        cx(&fake, "r1").with_progress(Arc::new(move |m| log.lock().unwrap().push(m.to_owned())));
    let w = workload(Some(atuin_job("atuin:18.6.1")));

    let plan = w.plan(&cx).await.unwrap();
    assert!(matches!(
        &plan.steps[0].planned,
        Planned::Change {
            diff: Diff::Create { .. }
        }
    ));
    plan.apply(&cx).await.unwrap();
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("1/1 healthy")),
        "{seen:?}"
    );

    let live = fake.job("default", "atuin").unwrap();
    let s = stamps::Stamps::from_meta(live.get("Meta")).unwrap();
    assert_eq!(
        (s.target.as_str(), s.module.as_str()),
        ("home-nomad", "atuin")
    );
    assert_eq!(job_revision(&fake).await, "r1");
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);

    let checks = health::job(&fake.client(), "default", "atuin")
        .await
        .unwrap();
    assert!(checks.iter().all(|c| c.status.is_pass()), "{checks:?}");
}

#[tokio::test]
async fn a_revision_that_does_not_touch_a_job_leaves_its_stamps_alone() {
    let fake = FakeNomad::new();
    let w = workload(Some(atuin_job("atuin:1")));
    w.plan(&cx(&fake, "r1"))
        .await
        .unwrap()
        .apply(&cx(&fake, "r1"))
        .await
        .unwrap();

    assert_eq!(w.plan(&cx(&fake, "r2")).await.unwrap().changes(), 0);

    let changed = workload(Some(atuin_job("atuin:2")));
    let plan = changed.plan(&cx(&fake, "r3")).await.unwrap();
    assert!(matches!(
        &plan.steps[0].planned,
        Planned::Change {
            diff: Diff::Update { .. }
        }
    ));
    plan.apply(&cx(&fake, "r3")).await.unwrap();
    assert_eq!(job_revision(&fake).await, "r3");
}

async fn job_revision(fake: &FakeNomad) -> String {
    index::read(&fake.client(), "home-nomad")
        .await
        .unwrap()
        .find(Kind::Job, "default", "atuin")
        .expect("the job is listed in the index")
        .revision
        .clone()
}

#[tokio::test]
async fn a_count_change_leaves_the_jobs_meta_untouched() {
    let fake = FakeNomad::new();
    let scaled = |count: u64| {
        job(serde_json::json!({
            "ID": "atuin", "Type": "service",
            "TaskGroups": [{"Name": "api", "Count": count}],
        }))
    };
    let cx1 = cx(&fake, "r1");
    workload(Some(scaled(1)))
        .plan(&cx1)
        .await
        .unwrap()
        .apply(&cx1)
        .await
        .unwrap();
    let before = fake.job("default", "atuin").unwrap()["Meta"].clone();

    let cx2 = cx(&fake, "r2");
    workload(Some(scaled(2)))
        .plan(&cx2)
        .await
        .unwrap()
        .apply(&cx2)
        .await
        .unwrap();
    let after = fake.job("default", "atuin").unwrap();
    assert_eq!(after["TaskGroups"][0]["Count"], 2);
    assert_eq!(after["Meta"], before);
    assert!(after["Meta"].get(stamps::REVISION).is_none());
    assert_eq!(job_revision(&fake).await, "r2");
}

#[tokio::test]
async fn a_job_changed_between_plan_and_apply_is_not_overwritten() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    workload(Some(atuin_job("atuin:1")))
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();

    let plan = workload(Some(atuin_job("atuin:2")))
        .plan(&cx)
        .await
        .unwrap();
    fake.edit_job("default", "atuin", |j| j["Priority"] = 70.into());
    let err = plan.apply(&cx).await.unwrap_err();
    assert!(
        err.message().contains("changed in the cluster"),
        "{}",
        err.message()
    );
    assert_eq!(fake.job("default", "atuin").unwrap()["Priority"], 70);
}

#[tokio::test]
async fn a_failed_deployment_fails_the_apply() {
    let fake = FakeNomad::new();
    fake.deployment_outcome("failed");
    let cx = cx(&fake, "r1");
    let err = workload(Some(atuin_job("atuin:1")))
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap_err();
    assert!(
        err.message()
            .contains("deployment of job default/atuin failed"),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn objects_of_another_target_block_the_plan() {
    let fake = FakeNomad::new();
    let w = workload(Some(atuin_job("atuin:1")));
    let other = Context {
        target: "lab-nomad".into(),
        ..cx(&fake, "r1")
    };
    w.plan(&other).await.unwrap().apply(&other).await.unwrap();

    let plan = w.plan(&cx(&fake, "r1")).await.unwrap();
    let (_, reason) = plan.blocked().expect("blocked");
    assert!(reason.contains("lab-nomad"), "{reason}");
    assert!(plan.apply(&cx(&fake, "r1")).await.is_err());
}

#[tokio::test]
async fn a_job_in_a_namespace_created_by_the_same_workload_plans_as_a_registration() {
    let fake = FakeNomad::new();
    let mut spec = atuin_job("atuin:1");
    spec.hcl = spec.hcl.replacen('{', "{\"Namespace\":\"team-a\",", 1);
    let w = with(vec![
        Primitive::Entity(EntitySpec::new(
            EntityKind::Namespace,
            "team-a.namespace.hcl",
            "name = \"team-a\"\ndescription = \"Team A\"",
        )),
        Primitive::Job(spec),
    ]);
    let cx = cx(&fake, "r1");
    let plan = w.plan(&cx).await.unwrap();
    assert_eq!(
        plan.keys()[0],
        ObjectKey::new(Kind::Namespace, "", "team-a")
    );
    assert!(plan.steps[1].planned.is_change());
    assert!(!fake.requests().iter().any(|(_, p)| p.contains("/plan")));
    plan.apply(&cx).await.unwrap();
    assert!(fake.job("team-a", "atuin").is_some());
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
}

#[tokio::test]
async fn variables_plan_by_fingerprint_and_never_show_secret_values() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = with(vec![Primitive::Variable(variables(&[
        (
            "DATABASE_URL",
            render("postgres://atuin:{{ secrets.db }}@db/atuin"),
        ),
        ("OPEN_REGISTRATION", Rendered::Plain("false".into())),
    ]))]);
    let plan = w.plan(&cx).await.unwrap();
    let shown = format!(
        "{:?} {}",
        plan.steps,
        serde_json::to_string(&plan.steps[0].planned).unwrap()
    );
    assert_no_secret(&shown);
    assert!(
        shown.contains("OPEN_REGISTRATION = \\\"false\\\""),
        "{shown}"
    );
    assert!(shown.contains("DATABASE_URL = \u{ab}value@"), "{shown}");
    plan.apply(&cx).await.unwrap();
    assert_eq!(
        fake.variable("default", "nomad/jobs/atuin").unwrap()["DATABASE_URL"],
        format!("postgres://atuin:{SECRET}@db/atuin")
    );
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
    let idx = index::read(&fake.client(), "home-nomad").await.unwrap();
    assert_eq!(
        idx.entries[0].items,
        vec!["DATABASE_URL", "OPEN_REGISTRATION"]
    );
    assert_no_secret(&format!("{:?}", idx.entries));
}

#[tokio::test]
async fn items_operators_add_stay_and_items_dropped_from_the_workload_go() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let both = with(vec![Primitive::Variable(variables(&[
        ("A", Rendered::Plain("1".into())),
        ("B", Rendered::Plain("2".into())),
    ]))]);
    both.plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    let mut items: Vec<(String, String)> = fake
        .variable("default", "nomad/jobs/atuin")
        .unwrap()
        .into_iter()
        .collect();
    items.push(("OPERATOR".into(), "x".into()));
    let items: Vec<(&str, &str)> = items
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    fake.put_variable("default", "nomad/jobs/atuin", &items);
    assert_eq!(both.plan(&cx).await.unwrap().changes(), 0);

    let only_a = with(vec![Primitive::Variable(variables(&[(
        "A",
        Rendered::Plain("1".into()),
    )]))]);
    let plan = only_a.plan(&cx).await.unwrap();
    let Planned::Change {
        diff: Diff::Update {
            unified: Some(text),
            ..
        },
    } = &plan.steps[0].planned
    else {
        panic!("{:?}", plan.steps[0].planned);
    };
    assert!(text.get().contains("- B"), "{}", text.get());
    plan.apply(&cx).await.unwrap();
    let live = fake.variable("default", "nomad/jobs/atuin").unwrap();
    assert_eq!(live.keys().collect::<Vec<_>>(), vec!["A", "OPERATOR"]);
}

#[tokio::test]
async fn a_variable_changed_after_its_plan_is_refused_without_echoing_values() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    fake.put_variable("default", "nomad/jobs/atuin", &[("DATABASE_URL", "old")]);
    let w = with(vec![Primitive::Variable(variables(&[(
        "DATABASE_URL",
        render("{{ secrets.db }}"),
    )]))]);
    let plan = w.plan(&cx).await.unwrap();
    fake.put_variable("default", "nomad/jobs/atuin", &[("DATABASE_URL", SECRET)]);
    let err = plan.apply(&cx).await.unwrap_err();
    assert!(
        err.message().contains("changed in the cluster"),
        "{}",
        err.message()
    );
    assert_no_secret(&format!("{} {}", err.message(), err.description()));
}

#[tokio::test]
async fn policies_namespaces_and_pools_are_stamped_and_converge() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = with(vec![
        Primitive::Policy(PolicySpec {
            file: "atuin.policy.hcl".into(),
            rules: "namespace \"default\" { variables { path \"nomad/jobs/atuin\" { capabilities = [\"read\"] } } }\n".into(),
            attach: Some(Attach {
                job: "atuin".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        Primitive::Entity(EntitySpec::new(
            EntityKind::NodePool,
            "pis.pool.hcl",
            "node_pool \"pis\" {\n  description = \"Pis\"\n}\n",
        )),
    ]);
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    let policy = fake.entity("policy", "atuin").unwrap();
    assert!(
        policy["Description"]
            .as_str()
            .unwrap()
            .contains("[buckle target=home-nomad")
    );
    assert_eq!(policy["JobACL"]["JobID"], "atuin");
    assert_eq!(
        fake.entity("pool", "pis").unwrap()["Meta"][stamps::TARGET],
        "home-nomad"
    );
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
}

#[tokio::test]
async fn policies_go_through_the_acl_client_and_everything_else_does_not() {
    let fake = FakeNomad::new();
    let acl = FakeNomad::new();
    let cx = cx(&fake, "r1").with_acl(Some(acl.client()));
    let w = with(vec![
        Primitive::Policy(PolicySpec {
            file: "atuin.policy.hcl".into(),
            rules: "namespace \"default\" { policy = \"read\" }\n".into(),
            ..Default::default()
        }),
        volume(),
    ]);
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    assert!(acl.entity("policy", "atuin").is_some());
    assert!(fake.entity("policy", "atuin").is_none());
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
}

#[tokio::test]
async fn existing_unmanaged_objects_are_adopted_with_a_warning() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = with(vec![Primitive::Entity(EntitySpec::new(
        EntityKind::Namespace,
        "default.namespace.hcl",
        "name = \"default\"\ndescription = \"Default\"",
    ))]);
    let plan = w.plan(&cx).await.unwrap();
    assert!(
        plan.steps[0].warnings[0].contains("adopts"),
        "{:?}",
        plan.steps[0].warnings
    );
}

#[tokio::test]
async fn volumes_are_created_once_and_listed_in_the_index() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = with(vec![volume()]);
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    assert_eq!(fake.volumes().len(), 1);
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
    let idx = index::read(&fake.client(), "home-nomad").await.unwrap();
    assert_eq!(
        idx.entries[0].id,
        fake.volumes()[0]["ID"].as_str().map(str::to_owned)
    );
}

#[rstest::rstest]
#[case::stop(Removal::Stop, true, false, 1, true)]
#[case::purge(Removal::Purge, false, false, 0, false)]
#[case::keep(Removal::Keep, true, true, 1, true)]
#[tokio::test]
async fn orphans_are_found_from_stamps_and_the_index_and_removed_by_policy(
    #[case] removal: Removal,
    #[case] job_exists: bool,
    #[case] job_running: bool,
    #[case] volumes: usize,
    #[case] variable_exists: bool,
) {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = with(vec![
        volume(),
        Primitive::Variable(variables(&[("A", Rendered::Plain("1".into()))])),
        Primitive::Policy(PolicySpec {
            file: "atuin.policy.hcl".into(),
            rules: "namespace \"default\" {}\n".into(),
            ..Default::default()
        }),
        Primitive::Job(atuin_job("atuin:1")),
    ]);
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();

    let found = orphans(&cx, &[]).await.unwrap();
    let kinds: Vec<Kind> = found.iter().map(|o| o.key.kind).collect();
    assert_eq!(
        kinds,
        vec![Kind::Job, Kind::Policy, Kind::Variable, Kind::Volume]
    );
    assert!(found.iter().all(|o| o.module == "atuin"));
    for o in &found {
        o.remove(&cx, removal).await.unwrap();
    }
    let job = fake.job("default", "atuin");
    assert_eq!(job.is_some(), job_exists);
    assert_eq!(job.is_some_and(|j| j["Stop"] == false), job_running);
    assert_eq!(fake.volumes().len(), volumes);
    assert_eq!(
        fake.variable("default", "nomad/jobs/atuin").is_some(),
        variable_exists
    );
    if removal != Removal::Keep {
        assert_eq!(orphans(&cx, &[]).await.unwrap(), vec![]);
    }
}

#[tokio::test]
async fn a_job_without_a_namespace_lands_in_the_workloads() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let mut spec = atuin_job("atuin:1");
    spec.namespace = Some("team-a".into());
    workload(Some(spec))
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    assert!(fake.job("team-a", "atuin").is_some());
}

/// atuin's job with its image tag from `var.version`, optionally handed to
/// an updater with `live_vars`.
fn versioned_job(live: bool, version: &str) -> JobSpec {
    let mut spec = job(serde_json::json!({
        "ID": "atuin", "Type": "service", "Meta": {"autoupdate.version": "docker:atuin:latest"},
        "TaskGroups": [{"Name": "api", "Count": 1, "Tasks": [{"Name": "atuin", "Config": {"image": "atuin:${var.version}"}}]}],
    }));
    spec.vars
        .insert("version".into(), serde_json::json!(version));
    if live {
        spec.live_vars = vec!["version".into()];
    }
    spec
}

/// Re-registers the running job the way nomad-auto-update does: the image
/// pinned to a digest, the submission's `version` flag with it.
fn pin_by_updater(fake: &FakeNomad, pinned: &str) {
    let sub = fake.submission("default", "atuin").expect("a submission");
    let mut job = fake.job("default", "atuin").unwrap();
    job["TaskGroups"][0]["Tasks"][0]["Config"]["image"] = format!("atuin:{pinned}").into();
    let mut pinned_sub = sub.clone();
    pinned_sub["VariableFlags"]["version"] = pinned.into();
    fake.reregister("default", "atuin", job, pinned_sub);
}

#[rstest::rstest]
#[case::live_var(true, 0)]
#[case::not_live(false, 1)]
#[tokio::test]
async fn an_updaters_pin_is_kept_for_live_vars_only(#[case] live: bool, #[case] changes: usize) {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let w = workload(Some(versioned_job(live, "latest")));
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();

    // The source and values are kept for the updater to re-render.
    let sub = fake.submission("default", "atuin").expect("a submission");
    assert_eq!(sub["VariableFlags"]["version"], "latest");
    assert!(sub["Source"].as_str().unwrap().contains("${var.version}"));

    pin_by_updater(&fake, "latest@sha256:abc");
    assert_eq!(w.plan(&cx).await.unwrap().changes(), changes);
}

#[tokio::test]
async fn a_live_var_the_workload_changes_wins_once_then_the_updater_owns_it_again() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    workload(Some(versioned_job(true, "v1")))
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    pin_by_updater(&fake, "v1@sha256:abc");

    // The workload moves to v2: that wins over the pin.
    let w = workload(Some(versioned_job(true, "v2")));
    let plan = w.plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 1);
    plan.apply(&cx).await.unwrap();
    assert_eq!(
        fake.job("default", "atuin").unwrap()["TaskGroups"][0]["Tasks"][0]["Config"]["image"],
        "atuin:v2"
    );
    let idx = index::read(&fake.client(), "home-nomad").await.unwrap();
    assert_eq!(
        idx.find(Kind::Job, "default", "atuin").unwrap().live_vars["version"],
        "v2"
    );

    // The updater pins v2; the unchanged workload leaves the pin alone.
    pin_by_updater(&fake, "v2@sha256:def");
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
}

#[tokio::test]
async fn a_live_var_the_spec_does_not_declare_is_a_warning() {
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let mut spec = atuin_job("atuin:1");
    spec.live_vars = vec!["nope".into()];
    let plan = workload(Some(spec)).plan(&cx).await.unwrap();
    assert!(
        plan.steps[0].warnings.iter().any(|w| w.contains("'nope'")),
        "{:?}",
        plan.steps[0].warnings
    );
}

#[tokio::test]
async fn ignored_fields_take_the_live_value_in_plans_and_applies() {
    use buckle_core::fieldpath::FieldPath;
    let fake = FakeNomad::new();
    let cx = cx(&fake, "r1");
    let mut spec = atuin_job("atuin:1");
    spec.ignore = vec![FieldPath::parse("TaskGroups[*].Tasks[*].Config.image").unwrap()];
    let w = workload(Some(spec));
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();

    // Another tool changes the image: not a change, and an apply after an
    // unrelated edit keeps it.
    fake.edit_job("default", "atuin", |j| {
        j["TaskGroups"][0]["Tasks"][0]["Config"]["image"] = "atuin:1@sha256:abc".into();
    });
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);

    let mut bumped = atuin_job("atuin:1");
    bumped.hcl = bumped.hcl.replacen("\"Count\":1", "\"Count\":2", 1);
    bumped.ignore = vec![FieldPath::parse("TaskGroups[*].Tasks[*].Config.image").unwrap()];
    let plan = workload(Some(bumped)).plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 1);
    plan.apply(&cx).await.unwrap();
    let live = fake.job("default", "atuin").unwrap();
    assert_eq!(live["TaskGroups"][0]["Count"], 2);
    assert_eq!(
        live["TaskGroups"][0]["Tasks"][0]["Config"]["image"],
        "atuin:1@sha256:abc"
    );
}

#[tokio::test]
async fn concurrent_index_writes_through_one_client_all_land() {
    let fake = FakeNomad::new();
    let nomad = fake.client();
    let mut writes = tokio::task::JoinSet::new();
    for i in 0..16 {
        let nomad = nomad.clone();
        writes.spawn(async move {
            let entry = index::Entry {
                kind: Kind::Variable,
                namespace: "default".into(),
                name: format!("v{i}"),
                module: format!("w{i}"),
                revision: "r1".into(),
                digest: String::new(),
                items: Vec::new(),
                id: None,
                live_vars: Default::default(),
            };
            index::update(&nomad, "home", |e| index::upsert(e, entry.clone())).await
        });
    }
    while let Some(result) = writes.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(index::read(&nomad, "home").await.unwrap().entries.len(), 16);
}

#[rstest::rstest]
#[case::placed_once_the_port_frees(3, None)]
#[case::never_placed(usize::MAX, Some("could not be placed: api: cannot place"))]
#[tokio::test(start_paused = true)]
async fn a_blocked_placement_is_waited_for(#[case] polls: usize, #[case] error: Option<&str>) {
    let fake = FakeNomad::new();
    fake.block_placement(polls);
    let cx = cx(&fake, "r1");
    let applied = workload(Some(atuin_job("atuin:1")))
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await;
    match error {
        None => assert!(applied.is_ok(), "{:?}", applied.err().map(|e| e.message())),
        Some(text) => {
            let message = applied.unwrap_err().message();
            assert!(message.contains(text), "{message}");
        }
    }
}
