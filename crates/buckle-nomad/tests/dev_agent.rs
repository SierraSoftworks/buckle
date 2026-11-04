//! The primitives against a real Nomad: a dev agent started by
//! `scripts/nomad-dev-agent.sh` (CI runs it on the x64 Linux job).
//!
//! Skipped unless `BUCKLE_NOMAD_ADDR` is set; `BUCKLE_NOMAD_TOKEN` is the
//! management token when the agent has ACLs enabled. Each test uses its own
//! target and object names, so they run in parallel.

use buckle_core::health::HealthStatus;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::{InstallKey, Rendered};
use buckle_nomad::primitives::*;
use buckle_nomad::{Nomad, Removal, Workload, health, index, stamps};
use std::sync::Arc;

fn nomad() -> Option<Nomad> {
    let addr = std::env::var("BUCKLE_NOMAD_ADDR").ok()?;
    let mut nomad = Nomad::connect(&addr, None).expect("BUCKLE_NOMAD_ADDR is a Nomad address");
    if let Ok(token) = std::env::var("BUCKLE_NOMAD_TOKEN") {
        nomad = nomad.token(token.into());
    }
    Some(nomad)
}

macro_rules! nomad_or_skip {
    () => {
        match nomad() {
            Some(n) => n,
            None => {
                eprintln!(
                    "BUCKLE_NOMAD_ADDR is not set; skipping (run scripts/nomad-dev-agent.sh)"
                );
                return;
            }
        }
    };
}

fn cx(nomad: &Nomad, target: &str, revision: &str) -> Context {
    Context::new(
        nomad.clone(),
        target,
        "it",
        revision,
        Arc::new(InstallKey::from_bytes([5; 32])),
    )
}

const JOB: &str = r#"
variable "sleep" {
  type = string
}

variable "count" {
  type    = number
  default = 1
}

job "JOB_ID" {
  datacenters = ["*"]
  type        = "service"

  update {
    min_healthy_time  = "1s"
    healthy_deadline  = "1m"
    progress_deadline = "2m"
  }

  group "api" {
    count = var.count

    task "sleep" {
      driver = "raw_exec"
      config {
        command = "/bin/sleep"
        args    = [var.sleep]
      }
      resources {
        cpu    = 20
        memory = 16
      }
    }
  }
}
"#;

fn job(id: &str, sleep: &str) -> JobSpec {
    JobSpec {
        file: format!("{id}.nomad.hcl"),
        hcl: JOB.replace("JOB_ID", id),
        // A fleet-wide var the job does not declare, as an unconditional
        // vars overlay gives every target: Nomad refuses it, so it is left out.
        vars: [
            ("sleep".to_owned(), serde_json::json!(sleep)),
            ("nomad_region".to_owned(), serde_json::json!("global")),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_job_deploys_converges_updates_and_stops() {
    let nomad = nomad_or_skip!();
    let target = "it-job";
    let w = |sleep: &str| Workload {
        name: "it".into(),
        job: Some(job("buckle-it-job", sleep)),
        ..Default::default()
    };

    let plan = w("3600").plan(&cx(&nomad, target, "r1")).await.unwrap();
    assert!(
        matches!(
            &plan.steps[0].planned,
            Planned::Change {
                diff: Diff::Create { .. }
            }
        ),
        "{:?}",
        plan.steps[0]
    );
    assert!(
        plan.steps[0]
            .preview
            .iter()
            .any(|l| l.contains("api: 1 create")),
        "{:?}",
        plan.steps[0].preview
    );
    plan.apply(&cx(&nomad, target, "r1")).await.unwrap();

    let checks = health::job(&nomad, "default", "buckle-it-job")
        .await
        .unwrap();
    assert!(checks.iter().all(|c| c.status.is_pass()), "{checks:?}");
    assert_eq!(
        w("3600")
            .plan(&cx(&nomad, target, "r2"))
            .await
            .unwrap()
            .changes(),
        0
    );

    let update = w("3601").plan(&cx(&nomad, target, "r3")).await.unwrap();
    let Planned::Change {
        diff: Diff::Update {
            unified: Some(text),
            ..
        },
    } = &update.steps[0].planned
    else {
        panic!("{:?}", update.steps[0].planned);
    };
    assert!(text.get().contains("3601"), "{}", text.get());
    update.apply(&cx(&nomad, target, "r3")).await.unwrap();
    let idx = index::read(&nomad, target).await.unwrap();
    assert_eq!(
        idx.find(Kind::Job, "default", "buckle-it-job")
            .unwrap()
            .revision,
        "r3"
    );
    let live: serde_json::Value = nomad.get("/v1/job/buckle-it-job").await.unwrap().unwrap();
    assert!(
        live["Meta"].get(stamps::REVISION).is_none(),
        "{}",
        live["Meta"]
    );

    // A count change places one allocation and leaves the running one alone.
    let mut scaled = w("3601");
    scaled
        .job
        .as_mut()
        .unwrap()
        .vars
        .insert("count".into(), serde_json::json!(2));
    let plan = scaled.plan(&cx(&nomad, target, "r4")).await.unwrap();
    let preview = plan.steps[0].preview.join("; ");
    assert!(
        preview.contains("1 create") && !preview.contains("create/destroy"),
        "{preview}"
    );
    plan.apply(&cx(&nomad, target, "r4")).await.unwrap();

    let found = orphans(&cx(&nomad, target, "r4"), &[]).await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    found[0]
        .remove(&cx(&nomad, target, "r4"), Removal::Purge)
        .await
        .unwrap();
    let checks = health::job(&nomad, "default", "buckle-it-job")
        .await
        .unwrap();
    assert!(matches!(checks[0].status, HealthStatus::Fail { .. }));
}

#[tokio::test]
async fn a_job_changed_after_its_plan_is_refused() {
    let nomad = nomad_or_skip!();
    let cx = cx(&nomad, "it-enforce", "r1");
    let w = |sleep: &str| Workload {
        name: "it".into(),
        job: Some(job("buckle-it-enforce", sleep)),
        ..Default::default()
    };
    w("3600").plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    let stale = w("3601").plan(&cx).await.unwrap();
    w("3602").plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    let err = stale.apply(&cx).await.unwrap_err();
    assert!(
        err.message().contains("changed in the cluster"),
        "{}",
        err.message()
    );
    Orphan {
        key: ObjectKey::new(Kind::Job, "default", "buckle-it-enforce"),
        module: "it".into(),
        id: None,
    }
    .remove(&cx, Removal::Purge)
    .await
    .unwrap();
}

#[tokio::test]
async fn variables_policies_namespaces_and_pools_converge_and_purge() {
    let nomad = nomad_or_skip!();
    let cx = cx(&nomad, "it-entities", "r1");
    let w = Workload {
        name: "it".into(),
        namespaces: vec![EntitySpec {
            kind: EntityKind::Namespace,
            file: "it.namespace.hcl".into(),
            hcl: "name = \"buckle-it\"\ndescription = \"Buckle integration tests\"\nmeta { owner = \"buckle\" }\n".into(),
        }],
        node_pools: vec![EntitySpec {
            kind: EntityKind::NodePool,
            file: "it.pool.hcl".into(),
            hcl: "node_pool \"buckle-it\" {\n  description = \"Buckle integration tests\"\n}\n".into(),
        }],
        variables: vec![VariableSpec {
            namespace: "buckle-it".into(),
            path: "nomad/jobs/buckle-it".into(),
            items: [("TZ".to_owned(), Rendered::Plain("Europe/London".into()))].into_iter().collect(),
        }],
        policies: vec![PolicySpec {
            file: "buckle-it.policy.hcl".into(),
            rules: "namespace \"buckle-it\" {\n  variables {\n    path \"nomad/jobs/buckle-it\" {\n      capabilities = [\"read\"]\n    }\n  }\n}\n".into(),
            description: "Integration test policy".into(),
            attach: Some(Attach {
                namespace: Some("buckle-it".into()),
                job: "buckle-it".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let plan = w.plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 4, "{:?}", plan.steps);
    plan.apply(&cx).await.unwrap();
    let again = w.plan(&cx).await.unwrap();
    assert_eq!(again.changes(), 0, "{:?}", again.steps);

    let idx = index::read(&nomad, "it-entities").await.unwrap();
    assert_eq!(idx.entries.len(), 1);

    let found = orphans(&cx, &[]).await.unwrap();
    assert_eq!(found.len(), 4, "{found:?}");
    for o in &found {
        o.remove(&cx, Removal::Purge).await.unwrap();
    }
    assert_eq!(orphans(&cx, &[]).await.unwrap(), vec![]);
}

#[tokio::test]
async fn a_host_volume_is_created_once() {
    let nomad = nomad_or_skip!();
    let cx = cx(&nomad, "it-volume", "r1");
    let w = Workload {
        name: "it".into(),
        volumes: vec![VolumeSpec {
            file: "it.volume.hcl".into(),
            hcl: r#"
name      = "buckle-it"
type      = "host"
plugin_id = "mkdir"
capability {
  access_mode     = "single-node-single-writer"
  attachment_mode = "file-system"
}
"#
            .into(),
        }],
        ..Default::default()
    };
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
    let found = orphans(&cx, &[]).await.unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].id.is_some());
    found[0].remove(&cx, Removal::Purge).await.unwrap();
}
