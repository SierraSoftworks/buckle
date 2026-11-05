//! `job`: an HCL2 job spec, sent verbatim to `/v1/jobs/parse` with the
//! workload's vars as HCL variables, planned with `/v1/job/:id/plan` and
//! registered with `EnforceIndex` on the index the plan saw.

use super::{Context, Kind, ObjectKey, Orphan, Pending as StepPending, Step, namespace_or_default};
use crate::client::{seg, with_query};
use crate::diff;
use crate::index::{self, Entry};
use crate::stamps::{self, Owner, Stamps};
use buckle_core::Result;
use buckle_core::error::{system, user};
use buckle_core::fieldpath::{FieldPath, adopt_all};
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
    /// The workload's `namespace`, used when the spec names none. Nomad
    /// canonicalises a missing namespace to `default`, so a spec that names
    /// `default` explicitly also lands here.
    pub namespace: Option<String>,
    /// Variables another tool owns once the job runs (nomad-auto-update's
    /// pinned digests and tags): the live submission's value is the input,
    /// unless the workload's own value changed since Buckle last wrote it.
    pub live_vars: Vec<String>,
    /// Field paths of the live job that may differ from the spec: the live
    /// value is adopted before the plan, so neither the diff nor the apply
    /// touches them.
    pub ignore: Vec<FieldPath>,
    /// How long the apply waits on a blocked placement; `None`:
    /// [`PLACEMENT_GRACE`].
    pub placement_timeout: Option<Duration>,
    /// How long the apply follows the deployment; `None`:
    /// [`DEPLOYMENT_TIMEOUT`].
    pub rollout_timeout: Option<Duration>,
}

impl JobSpec {
    /// The vars the spec declares a `variable` for. Fleet overlays apply to
    /// every target, and Nomad refuses a value for an undeclared variable, so
    /// the rest are left out. A spec that does not parse here keeps them all
    /// and lets Nomad's parser report the problem.
    pub fn declared_vars(&self) -> IndexMap<String, Value> {
        let Ok(body) = hcl::parse(&self.hcl) else {
            return self.vars.clone();
        };
        let declared: std::collections::HashSet<&str> = body
            .blocks()
            .filter(|b| b.identifier() == "variable")
            .filter_map(|b| b.labels().first().map(|l| l.as_str()))
            .collect();
        self.vars
            .iter()
            .filter(|(name, _)| declared.contains(name.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    job: Value,
    /// The source and variable values registered with the job, so that Nomad
    /// keeps them (`/v1/job/:id/submission`) for tools that re-render it.
    submission: Value,
    /// The job modify index the plan saw; 0 when the job must not exist.
    enforce_index: u64,
    /// The job's revision and digest, recorded in the index on apply.
    entry: Entry,
    waits: Waits,
}

/// How long an apply waits on the scheduler.
#[derive(Debug, Clone, Copy)]
pub struct Waits {
    /// On a blocked placement.
    pub placement: Duration,
    /// On the whole: placement and deployment.
    pub rollout: Duration,
}

impl Waits {
    fn of(spec: &JobSpec) -> Self {
        Self {
            placement: spec.placement_timeout.unwrap_or(PLACEMENT_GRACE),
            rollout: spec.rollout_timeout.unwrap_or(DEPLOYMENT_TIMEOUT),
        }
    }
}

/// How often an apply polls the evaluation and the deployment.
const POLL: Duration = Duration::from_secs(2);

/// The longest an apply follows a deployment. Nomad fails a deployment that
/// makes no progress within its `progress_deadline` well before this.
pub const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(3600);

/// How long an apply waits on a blocked evaluation before it reports the
/// job as unplaceable. A destructive update of a job with a static port
/// blocks until the old allocation lets go of the port, usually within its
/// `kill_timeout`.
pub const PLACEMENT_GRACE: Duration = Duration::from_secs(300);

/// The values the spec is rendered with: its declared vars, its files, and
/// `pinned` over both.
fn variables(spec: &JobSpec, pinned: &IndexMap<String, Value>) -> IndexMap<String, Value> {
    let mut vars = spec.declared_vars();
    for (name, content) in &spec.files {
        vars.insert(name.clone(), Value::String(content.clone()));
    }
    for (name, value) in pinned {
        vars.insert(name.clone(), value.clone());
    }
    vars
}

/// The job's submission for Nomad to keep: what `nomad job run -var` records.
fn submission(spec: &JobSpec, vars: &IndexMap<String, Value>) -> Value {
    let flags: serde_json::Map<String, Value> = vars
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(crate::hcl::flag(v))))
        .collect();
    json!({"Format": "hcl2", "Source": spec.hcl, "VariableFlags": flags})
}

/// What the plan found out about the job's `live_vars`.
struct LiveVars {
    /// The values to render with, over the workload's.
    pinned: IndexMap<String, Value>,
    /// The workload's own value of each live var, as a `-var` flag, for the
    /// index: the next plan compares against it to tell a change the
    /// workload made from a value another tool pinned.
    written: std::collections::BTreeMap<String, String>,
    warnings: Vec<String>,
}

/// Resolves the spec's `live_vars` against the live version's submission
/// and the index. A live var whose workload value equals what Buckle last
/// wrote (or has no record yet) takes the live value; one the workload
/// changed takes the workload's, so an operator's edit wins once and the
/// tool owns the variable again from there.
async fn live_vars(
    cx: &Context,
    spec: &JobSpec,
    live: Option<&Value>,
    ns: &str,
    id: &str,
    previous: Option<&Entry>,
) -> Result<LiveVars> {
    let declared = spec.declared_vars();
    let mut out = LiveVars {
        pinned: IndexMap::new(),
        written: Default::default(),
        warnings: Vec::new(),
    };
    if spec.live_vars.is_empty() {
        return Ok(out);
    }
    let declares = |name: &str| {
        hcl::parse(&spec.hcl).is_ok_and(|body| {
            body.blocks().any(|b| {
                b.identifier() == "variable"
                    && b.labels().first().is_some_and(|l| l.as_str() == name)
            })
        })
    };
    let mut flags: Option<Value> = None;
    if let Some(live) = live {
        let version = live["Version"].as_u64().unwrap_or(0).to_string();
        let url = with_query(
            &with_query(&format!("/v1/job/{}/submission", seg(id)), "namespace", ns),
            "version",
            &version,
        );
        flags = cx
            .nomad
            .get::<Value>(&url)
            .await?
            .map(|sub| sub["VariableFlags"].clone());
    }
    for name in &spec.live_vars {
        if !declared.contains_key(name) && !declares(name) {
            out.warnings.push(format!(
                "live_vars names '{name}', which {} declares no variable for.",
                spec.file
            ));
            continue;
        }
        let workload = declared.get(name).map(crate::hcl::flag);
        if let Some(w) = &workload {
            out.written.insert(name.clone(), w.clone());
        }
        let live_value = flags
            .as_ref()
            .and_then(|f| f[name].as_str())
            .map(str::to_owned);
        let Some(live_value) = live_value else {
            continue;
        };
        let last = previous.and_then(|e| e.live_vars.get(name));
        let changed_in_workload = matches!((last, &workload), (Some(l), Some(w)) if l != w);
        if !changed_in_workload {
            out.pinned.insert(name.clone(), Value::String(live_value));
        }
    }
    Ok(out)
}

async fn parse(cx: &Context, spec: &JobSpec, vars: &IndexMap<String, Value>) -> Result<Value> {
    let body = json!({
        "JobHCL": spec.hcl,
        "Variables": crate::hcl::var_file(vars)?,
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
    let mut vars = variables(spec, &IndexMap::new());
    let mut job = parse(cx, spec, &vars).await?;
    let id = job["ID"].as_str().unwrap_or_default().to_owned();
    if id.is_empty() {
        return Err(user(
            format!("{} declares no job.", spec.file),
            &["A job spec holds exactly one `job \"name\" { … }` block."],
        ));
    }
    let ns = match (job["Namespace"].as_str(), spec.namespace.as_deref()) {
        (None | Some("") | Some("default"), Some(workload)) if !workload.is_empty() => {
            workload.to_owned()
        }
        (named, _) => namespace_or_default(named),
    };
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
    let idx = index::read(&cx.nomad, &cx.target).await?;
    let previous = idx.find(Kind::Job, &ns, &id);
    let owned = live_vars(cx, spec, live.as_ref(), &ns, &id, previous).await?;
    let mut warnings = owned.warnings;
    if !owned.pinned.is_empty() {
        vars = variables(spec, &owned.pinned);
        job = parse(cx, spec, &vars).await?;
        job["Namespace"] = Value::String(ns.clone());
    }
    if let Some(live) = &live {
        adopt_all(&spec.ignore, &mut job, live);
    }
    let submission = submission(spec, &vars);
    // Only the owner stamps go in the job's meta: Nomad passes job meta to
    // every task, so a changing revision or digest there would turn every
    // change (a count bump included) into a destructive update. The revision
    // and digest live in the target's index instead.
    Stamps::stamp_owner(&cx.target, &cx.module, &mut job);
    let digest = stamps::short_digest(&(&spec.hcl, &spec.declared_vars(), &spec.files));
    let entry = Entry {
        kind: Kind::Job,
        namespace: ns.clone(),
        name: id.clone(),
        module: cx.module.clone(),
        revision: match previous {
            Some(p) if p.digest == digest && p.module == cx.module => p.revision.clone(),
            _ => cx.revision.clone(),
        },
        digest,
        items: Vec::new(),
        id: None,
        live_vars: owned.written,
    };

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
                submission,
                enforce_index: 0,
                entry,
                waits: Waits::of(spec),
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
    let rendered = diff::render_job_diff(&plan["Diff"]);
    let groups = if preview.is_empty() {
        String::new()
    } else {
        format!(" ({})", preview.join("; "))
    };
    let planned = match live {
        None => Planned::change(Diff::Create {
            summary: format!("register {key}{groups}"),
            // Every line is an addition: show the job without the markers.
            preview: Some(Redacted::plain(
                rendered
                    .lines()
                    .map(|l| l.get(1..).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
        }),
        Some(_) => Planned::change(Diff::Update {
            summary: format!("update {key}{groups}"),
            unified: Some(Redacted::plain(rendered)),
        }),
    };
    Ok(Step {
        key,
        planned,
        warnings,
        preview,
        pending: StepPending::Job(Pending {
            job,
            submission,
            enforce_index,
            entry,
            waits: Waits::of(spec),
        }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let url = with_query("/v1/jobs", "namespace", &key.namespace);
    let body = json!({
        "Job": p.job,
        "Submission": p.submission,
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
    (cx.progress)("registered; waiting for the deployment");
    let entry = p.entry.clone();
    index::update(&cx.nomad, &cx.target, |e| index::upsert(e, entry.clone())).await?;
    let outcome = follow_deployment(cx, key, &eval, modify, p.waits).await?;
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
    waits: Waits,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + waits.rollout;
    let ns = &key.namespace;
    if !eval_id.is_empty() {
        let mut eval_id = eval_id.to_owned();
        // A placement Nomad retries: why it failed, and when to stop waiting.
        let mut unplaced: Option<(String, tokio::time::Instant)> = None;
        let unplaceable = |why: &str| {
            user(
                format!("{key} could not be placed: {why}"),
                &["Check the job's constraints and the cluster's free resources."],
            )
        };
        loop {
            let url = with_query(
                &format!("/v1/evaluation/{}", seg(&eval_id)),
                "namespace",
                ns,
            );
            let eval = cx.nomad.get::<Value>(&url).await?.unwrap_or(Value::Null);
            match eval["Status"].as_str().unwrap_or("pending") {
                "complete" => {
                    let Some(failed) = eval["FailedTGAllocs"].as_object().filter(|f| !f.is_empty())
                    else {
                        break;
                    };
                    let why = failed
                        .iter()
                        .map(|(g, m)| format!("{g}: {}", diff::placement_failure(m)))
                        .collect::<Vec<_>>()
                        .join("; ");
                    // Nomad blocks an evaluation it could not place and
                    // retries it as capacity frees (a destructive update's
                    // old allocation releasing a static port, say).
                    let Some(blocked) = eval["BlockedEval"].as_str().filter(|b| !b.is_empty())
                    else {
                        return Err(unplaceable(&why));
                    };
                    (cx.progress)(&format!("{key}: waiting to be placed ({why})"));
                    let since = unplaced.map_or_else(tokio::time::Instant::now, |(_, at)| at);
                    unplaced = Some((why, since));
                    eval_id = blocked.to_owned();
                    continue;
                }
                "blocked" => {
                    if let Some((why, since)) = &unplaced
                        && since.elapsed() >= waits.placement
                    {
                        return Err(unplaceable(why));
                    }
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
            (cx.progress)(&format!("deploying: {progress}"));
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
    index::update(&cx.nomad, &cx.target, |e| {
        index::forget(e, Kind::Job, &key.namespace, &key.name)
    })
    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::declared_only(
        "variable \"image\" {\n  type = string\n}\njob \"paste\" {}\n",
        &["image"]
    )]
    #[case::unparseable_keeps_all("job \"paste\" {", &["image", "nomad_region"])]
    fn only_declared_vars_reach_nomad(#[case] hcl: &str, #[case] expected: &[&str]) {
        let spec = JobSpec {
            hcl: hcl.into(),
            vars: [("image", json!("tclip")), ("nomad_region", json!("global"))]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            ..Default::default()
        };
        let names: Vec<String> = spec.declared_vars().into_keys().collect();
        assert_eq!(names, expected);
    }
}
