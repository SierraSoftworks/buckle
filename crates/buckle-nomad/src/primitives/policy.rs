//! `policies`: ACL policies, optionally attached to a job's workload
//! identity (`nomad acl policy apply -job`). Stamped in the description.

use super::{Context, Kind, ObjectKey, Orphan, Pending as StepPending, Step};
use crate::client::seg;
use crate::stamps::{self, Owner, Stamps};
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::task::Applied;
use serde_json::{Value, json};

/// Attaches a policy to a job's workload identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attach {
    /// Defaults to `default`.
    pub namespace: Option<String>,
    pub job: String,
    pub group: Option<String>,
    pub task: Option<String>,
}

/// One policy file.
#[derive(Debug, Clone, Default)]
pub struct PolicySpec {
    pub file: String,
    /// The policy's rules (HCL), sent as written.
    pub rules: String,
    /// Defaults to the file name up to its first dot (`atuin.policy.hcl` → `atuin`).
    pub name: Option<String>,
    pub description: String,
    pub attach: Option<Attach>,
}

impl PolicySpec {
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let base = self.file.rsplit('/').next().unwrap_or(&self.file);
            base.split('.').next().unwrap_or(base).to_owned()
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    body: Value,
    modify_index: Option<u64>,
}

fn url(name: &str) -> String {
    format!("/v1/acl/policy/{}", seg(name))
}

fn job_acl(attach: &Option<Attach>) -> Value {
    match attach {
        None => Value::Null,
        Some(a) => json!({
            "Namespace": super::namespace_or_default(a.namespace.as_deref()),
            "JobID": a.job,
            "Group": a.group.clone().unwrap_or_default(),
            "Task": a.task.clone().unwrap_or_default(),
        }),
    }
}

fn same_acl(desired: &Value, live: &Value) -> bool {
    let norm = |v: &Value| -> Value {
        if v.is_null() || v["JobID"].as_str().unwrap_or_default().is_empty() {
            Value::Null
        } else {
            json!({
                "Namespace": v["Namespace"].as_str().unwrap_or("default"),
                "JobID": v["JobID"],
                "Group": v["Group"].as_str().unwrap_or_default(),
                "Task": v["Task"].as_str().unwrap_or_default(),
            })
        }
    };
    norm(desired) == norm(live)
}

pub(crate) async fn plan(cx: &Context, spec: &PolicySpec) -> Result<Step> {
    let name = spec.name();
    if name.is_empty() {
        return Err(user(
            format!("The policy in {} has no name.", spec.file),
            &["Name the policy, or name its file `<name>.policy.hcl`."],
        ));
    }
    if spec.attach.as_ref().is_some_and(|a| a.job.is_empty()) {
        return Err(user(
            format!("The policy {name} is attached without a job."),
            &["Set attach.job to the job whose workload identity gets the policy."],
        ));
    }
    let key = ObjectKey::new(Kind::Policy, "", &name);
    let live = cx.nomad.get::<Value>(&url(&name)).await?;
    let (_, live_stamps) = live
        .as_ref()
        .map(|l| Stamps::from_description(l["Description"].as_str().unwrap_or_default()))
        .unwrap_or_default();
    let owner = Owner::of(live_stamps, live.is_some(), &cx.target);
    if let Some(reason) = owner.conflict(&key.to_string()) {
        return Ok(Step::blocked(key, reason));
    }
    let acl = job_acl(&spec.attach);
    let digest = stamps::short_digest(&(&spec.rules, &spec.description, &acl));
    let description = owner
        .next(&cx.target, &cx.module, &cx.revision, &digest)
        .describe(&spec.description);
    let body = json!({
        "Name": name,
        "Description": description,
        "Rules": spec.rules,
        "JobACL": acl,
    });

    let mut warnings = Vec::new();
    if owner == Owner::Unmanaged {
        warnings.push(format!(
            "{key} exists and is not managed by Buckle; applying adopts it."
        ));
    }
    let attached = spec
        .attach
        .as_ref()
        .map(|a| format!(", attached to job {}", a.job))
        .unwrap_or_default();
    let (planned, modify_index) = match &live {
        None => (
            Planned::change(Diff::Create {
                summary: format!("create {key}{attached}"),
                preview: Some(Redacted::plain(spec.rules.clone())),
            }),
            None,
        ),
        Some(l) => {
            let rules_same = l["Rules"].as_str().unwrap_or_default().trim() == spec.rules.trim();
            let acl_same = same_acl(&acl, &l["JobACL"]);
            let desc_same = l["Description"].as_str().unwrap_or_default() == description;
            if rules_same && acl_same && desc_same {
                return Ok(Step::noop(key));
            }
            let mut text = String::new();
            if !rules_same {
                text = similar::TextDiff::from_lines(
                    l["Rules"].as_str().unwrap_or_default(),
                    &spec.rules,
                )
                .unified_diff()
                .header("current", "planned")
                .to_string();
            }
            if !acl_same {
                text.push_str(&format!("~ JobACL: {} => {}\n", l["JobACL"], acl));
            }
            if !desc_same {
                text.push_str(&format!(
                    "~ Description: {} => {:?}\n",
                    l["Description"], description
                ));
            }
            (
                Planned::change(Diff::Update {
                    summary: format!("update {key}{attached}"),
                    unified: Some(Redacted::plain(text)),
                }),
                l["ModifyIndex"].as_u64(),
            )
        }
    };
    Ok(Step {
        key,
        planned,
        warnings,
        preview: Vec::new(),
        pending: StepPending::Policy(Pending { body, modify_index }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let live = cx.nomad.get::<Value>(&url(&key.name)).await?;
    if live.as_ref().and_then(|l| l["ModifyIndex"].as_u64()) != p.modify_index {
        return Err(user(
            format!("{key} changed in the cluster after it was planned."),
            &["Plan again: the apply refuses to overwrite a change it has not shown."],
        ));
    }
    cx.nomad
        .write::<_, Value>("POST", &url(&key.name), &p.body)
        .await?;
    Ok(Applied::changed(format!("wrote {key}")))
}

pub(crate) async fn remove(cx: &Context, key: &ObjectKey) -> Result<Applied> {
    cx.nomad.delete(&url(&key.name)).await?;
    Ok(Applied::changed(format!("deleted {key}")))
}

pub(crate) async fn owned(cx: &Context) -> Result<Vec<Orphan>> {
    let list = cx
        .nomad
        .get::<Vec<Value>>("/v1/acl/policies")
        .await?
        .unwrap_or_default();
    Ok(list
        .into_iter()
        .filter_map(|p| {
            let (_, s) = Stamps::from_description(p["Description"].as_str().unwrap_or_default());
            let s = s.filter(|s| s.target == cx.target)?;
            Some(Orphan {
                key: ObjectKey::new(Kind::Policy, "", p["Name"].as_str().unwrap_or_default()),
                module: s.module,
                id: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("atuin.policy.hcl", "atuin")]
    #[case("policies/anonymous.policy.hcl", "anonymous")]
    fn names_default_to_the_file_stem(#[case] file: &str, #[case] name: &str) {
        let spec = PolicySpec {
            file: file.into(),
            ..Default::default()
        };
        assert_eq!(spec.name(), name);
    }

    #[test]
    fn attachments_compare_with_defaults_filled_in() {
        let desired = job_acl(&Some(Attach {
            job: "atuin".into(),
            ..Default::default()
        }));
        let live = json!({"Namespace": "default", "JobID": "atuin", "Group": "", "Task": ""});
        assert!(same_acl(&desired, &live));
        assert!(same_acl(&Value::Null, &json!({"JobID": ""})));
        assert!(!same_acl(&Value::Null, &live));
    }
}
