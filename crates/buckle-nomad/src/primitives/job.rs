//! `job`: an HCL2 job spec, sent verbatim to `/v1/jobs/parse` with the
//! workload's vars as HCL variables, planned with `/v1/job/:id/plan` and
//! registered with `EnforceIndex` on the index the plan saw.

use super::{Context, Kind, ObjectKey, Orphan, Pending as StepPending, Step, namespace_or_default};
use crate::client::{seg, with_query};
use crate::diff;
use crate::stamps::{self, Owner, Stamps};
use buckle_core::Result;
use buckle_core::error::{system, user};
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::task::Applied;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::time::Duration;

/// A job, as the workload declares it.
#[derive(Debug, Clone, Default)]
pub struct JobSpec {
    /// The spec's file name, for messages (`paste.nomad.hcl`).
    pub file: String,
    /// The HCL2 text, untouched.
    pub hcl: String,
    /// The workload's vars, passed as HCL variables (`nomad job run -var`).
    pub vars: IndexMap<String, Value>,
    /// Files passed as string variables (`files: { app_config: config.yaml }`).
    pub files: IndexMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    job: Value,
    /// The job modify index the plan saw; 0 when the job must not exist.
    enforce_index: u64,
}

/// How often an apply polls the evaluation and the deployment.
const POLL: Duration = Duration::from_secs(2);

/// The longest an apply follows a deployment. Nomad fails a deployment that
/// makes no progress within its `progress_deadline` well before this.
pub const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(3600);

async fn parse(cx: &Context, spec: &JobSpec) -> Result<Value> {
    let mut vars = spec.vars.clone();
    for (name, content) in &spec.files {
        vars.insert(name.clone(), Value::String(content.clone()));
    }
    let body = json!({
        "JobHCL": spec.hcl,
        "Variables": crate::hcl::var_file(&vars)?,
        "Canonicalize": true,
    });
    cx.nomad
        .write::<_, Value>("POST", "/v1/jobs/parse", &body)
        .await
        .map_err(|e| {
            user(
                format!("{} could not be parsed by Nomad: {}", spec.file, e.message()),
                &["Check the job spec with `nomad job validate` and that every variable it declares has a value."],
            )
        })
}

pub(crate) async fn plan(cx: &Context, spec: &JobSpec, created: &[ObjectKey]) -> Result<Step> {
    let mut job = parse(cx, spec).await?;
    let id = job["ID"].as_str().unwrap_or_default().to_owned();
    if id.is_empty() {
        return Err(user(
            format!("{} declares no job.", spec.file),
            &["A job spec holds exactly one `job \"name\" { … }` block."],
        ));
    }
    let ns = namespace_or_default(job["Namespace"].as_str());
    job["Namespace"] = Value::String(ns.clone());
    let key = ObjectKey::new(Kind::Job, &ns, &id);

    let job_url = with_query(&format!("/v1/job/{}", seg(&id)), "namespace", &ns);
    let live = cx.nomad.get::<Value>(&job_url).await?;
    let owner = Owner::of(
        live.as_ref().and_then(|j| Stamps::from_meta(j.get("Meta"))),
        live.is_some(),
        &cx.target,
    );
    if let Some(reason) = owner.conflict(&key.to_string()) {
        return Ok(Step::blocked(key, reason));
    }
    let digest = stamps::short_digest(&(&spec.hcl, &spec.vars, &spec.files));
    owner
        .next(&cx.target, &cx.module, &cx.revision, &digest)
        .stamp_meta(&mut job);

    let mut warnings = Vec::new();
    if owner == Owner::Unmanaged {
        warnings.push(format!(
            "{key} exists and is not managed by Buckle; applying adopts it."
        ));
    }

    let namespace_pending =
        live.is_none() && created.contains(&ObjectKey::new(Kind::Namespace, "", &ns));
    if namespace_pending {
        return Ok(Step {
            planned: Planned::change(Diff::Create {
                summary: format!("register {key} (its namespace is created first)"),
                preview: None,
            }),
            warnings,
            preview: Vec::new(),
            pending: StepPending::Job(Pending {
                job,
                enforce_index: 0,
            }),
            key,
        });
    }

    let plan_url = with_query(&format!("/v1/job/{}/plan", seg(&id)), "namespace", &ns);
    let plan: Value = cx
        .nomad
        .write("POST", &plan_url, &json!({"Job": job, "Diff": true}))
        .await?;
    warnings.extend(diff::plan_warnings(&plan));
    let preview = diff::scheduler_summary(&plan);
    let diff_type = plan["Diff"]["Type"].as_str().unwrap_or("None");
    if diff_type == "None" {
        return Ok(Step {
            warnings,
            preview,
            ..Step::noop(key)
        });
    }
    let enforce_index = plan["JobModifyIndex"].as_u64().unwrap_or(0);
    let rendered = Redacted::plain(diff::render_job_diff(&plan["Diff"]));
    let groups = if preview.is_empty() {
        String::new()
    } else {
        format!(" ({})", preview.join("; "))
    };
    let planned = match live {
        None => Planned::change(Diff::Create {
            summary: format!("register {key}{groups}"),
            preview: Some(rendered),
        }),
        Some(_) => Planned::change(Diff::Update {
            summary: format!("update {key}{groups}"),
            unified: Some(rendered),
        }),
    };
    Ok(Step {
        key,
        planned,
        warnings,
        preview,
        pending: StepPending::Job(Pending { job, enforce_index }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let url = with_query("/v1/jobs", "namespace", &key.namespace);
    let body = json!({
        "Job": p.job,
        "EnforceIndex": true,
        "JobModifyIndex": p.enforce_index,
    });
    let resp = cx
        .nomad
        .send(
            "POST",
            &url,
            Some(serde_json::to_vec(&body).unwrap_or_default()),
        )
        .await?;
    if resp.status != 200 && String::from_utf8_lossy(&resp.body).contains("modify index") {
        return Err(user(
            format!("{key} changed in the cluster after it was planned."),
            &["Plan again: the apply refuses to overwrite a change it has not shown."],
        ));
    }
    let resp = cx.nomad.ok("POST", &url, resp)?;
    let registered: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let eval = registered["EvalID"].as_str().unwrap_or_default().to_owned();
    let modify = registered["JobModifyIndex"].as_u64().unwrap_or(0);
    (cx.progress)(&format!("registered {key}"));
    let outcome = follow_deployment(cx, key, &eval, modify, DEPLOYMENT_TIMEOUT).await?;
    Ok(Applied::changed(format!("registered {key}; {outcome}")))
}

/// Follows the evaluation of a registration and the deployment it starts,
/// reporting progress until the deployment succeeds (`Ok`), fails or is
/// cancelled (`Err`). A job without a deployment (batch, system without
/// `update`, no changes to place) is done once its evaluation completes.
pub async fn follow_deployment(
    cx: &Context,
    key: &ObjectKey,
    eval_id: &str,
    job_modify_index: u64,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let ns = &key.namespace;
    if !eval_id.is_empty() {
        let url = with_query(&format!("/v1/evaluation/{}", seg(eval_id)), "namespace", ns);
        loop {
            let eval = cx.nomad.get::<Value>(&url).await?.unwrap_or(Value::Null);
            match eval["Status"].as_str().unwrap_or("pending") {
                "complete" => {
                    if let Some(failed) =
                        eval["FailedTGAllocs"].as_object().filter(|f| !f.is_empty())
                    {
                        let reasons: Vec<String> = failed
                            .iter()
                            .map(|(g, m)| format!("{g}: {}", diff::placement_failure(m)))
                            .collect();
                        return Err(user(
                            format!("{key} could not be placed: {}", reasons.join("; ")),
                            &["Check the job's constraints and the cluster's free resources."],
                        ));
                    }
                    break;
                }
                "failed" | "canceled" | "cancelled" => {
                    return Err(system(
                        format!(
                            "The evaluation of {key} {}: {}",
                            eval["Status"].as_str().unwrap_or("failed"),
                            eval["StatusDescription"]
                                .as_str()
                                .unwrap_or("no reason given")
                        ),
                        &["Check the Nomad servers' logs."],
                    ));
                }
                _ => {}
            }
            wait(deadline, key).await?;
        }
    }

    let dep_url = with_query(
        &format!("/v1/job/{}/deployment", seg(&key.name)),
        "namespace",
        ns,
    );
    let mut last = String::new();
    loop {
        let dep = cx
            .nomad
            .get::<Value>(&dep_url)
            .await?
            .unwrap_or(Value::Null);
        if dep.is_null()
            || dep["JobModifyIndex"]
                .as_u64()
                .is_some_and(|i| i != job_modify_index)
        {
            return Ok("no deployment".into());
        }
        let progress = deployment_progress(&dep);
        if progress != last {
            (cx.progress)(&format!("{key}: {progress}"));
            last = progress.clone();
        }
        match dep["Status"].as_str().unwrap_or("running") {
            "successful" => return Ok(format!("deployment successful ({progress})")),
            status @ ("failed" | "cancelled" | "canceled") => {
                return Err(user(
                    format!(
                        "The deployment of {key} {status}: {} ({progress})",
                        dep["StatusDescription"]
                            .as_str()
                            .unwrap_or("no reason given")
                    ),
                    &[
                        "Check the allocations' events in Nomad; the job's update block decides when a deployment fails.",
                    ],
                ));
            }
            _ => {}
        }
        wait(deadline, key).await?;
    }
}

/// `api 1/2 healthy, web 1/1 healthy`.
pub(crate) fn deployment_progress(dep: &Value) -> String {
    let Some(groups) = dep["TaskGroups"].as_object() else {
        return "no groups".into();
    };
    groups
        .iter()
        .map(|(g, s)| {
            let unhealthy = s["UnhealthyAllocs"].as_u64().unwrap_or(0);
            let mut text = format!(
                "{g} {}/{} healthy",
                s["HealthyAllocs"].as_u64().unwrap_or(0),
                s["DesiredTotal"].as_u64().unwrap_or(0)
            );
            if unhealthy > 0 {
                text.push_str(&format!(", {unhealthy} unhealthy"));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn wait(deadline: tokio::time::Instant, key: &ObjectKey) -> Result<()> {
    if tokio::time::Instant::now() + POLL > deadline {
        return Err(user(
            format!("{key} did not finish deploying in time."),
            &["Check the deployment in Nomad's UI; it carries on without the executor."],
        ));
    }
    tokio::time::sleep(POLL).await;
    Ok(())
}

pub(crate) async fn remove(cx: &Context, key: &ObjectKey, purge: bool) -> Result<Applied> {
    let mut url = with_query(
        &format!("/v1/job/{}", seg(&key.name)),
        "namespace",
        &key.namespace,
    );
    if purge {
        url = with_query(&url, "purge", "true");
    }
    cx.nomad.delete(&url).await?;
    Ok(Applied::changed(if purge {
        format!("stopped and purged {key}")
    } else {
        format!("stopped {key}")
    }))
}

/// The running jobs stamped by `cx.target`.
pub(crate) async fn owned(cx: &Context) -> Result<Vec<Orphan>> {
    let jobs = cx
        .nomad
        .get::<Vec<Value>>("/v1/jobs?namespace=*&meta=true")
        .await?
        .unwrap_or_default();
    Ok(jobs
        .into_iter()
        .filter(|j| !j["Stop"].as_bool().unwrap_or(false))
        .filter_map(|j| {
            let s = Stamps::from_meta(j.get("Meta"))?;
            (s.target == cx.target).then(|| Orphan {
                key: ObjectKey::new(
                    Kind::Job,
                    namespace_or_default(j["Namespace"].as_str()),
                    j["ID"].as_str().unwrap_or_default(),
                ),
                module: s.module,
                id: None,
            })
        })
        .collect())
}
