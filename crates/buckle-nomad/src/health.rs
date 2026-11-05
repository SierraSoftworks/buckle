//! Implicit health of a job, derived from its spec and the cluster's view:
//! nothing to write in the workload.
//!
//! * `job`: the job runs, every group has its desired count of running
//!   allocations of the current version, and the latest deployment of that
//!   version succeeded (service jobs); batch and system jobs are placed and
//!   not failing.
//! * `services`: every Nomad-native service check of the job's running
//!   allocations (not those being stopped) passes. Absent when the job
//!   registers none.

use crate::client::{Nomad, seg, with_query};
use crate::primitives::job::deployment_progress;
use buckle_core::Result;
use buckle_core::health::HealthStatus;
use serde_json::Value;
use std::collections::BTreeMap;

/// One implicit check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// `job` or `services`.
    pub name: &'static str,
    pub status: HealthStatus,
}

/// Runs the implicit checks of job `id` in namespace `ns`.
pub async fn job(nomad: &Nomad, ns: &str, id: &str) -> Result<Vec<Check>> {
    let job_url = with_query(&format!("/v1/job/{}", seg(id)), "namespace", ns);
    let Some(job) = nomad.get::<Value>(&job_url).await? else {
        return Ok(vec![Check {
            name: "job",
            status: HealthStatus::fail(format!("job {id} is not registered")),
        }]);
    };
    if job["Stop"].as_bool().unwrap_or(false) {
        return Ok(vec![Check {
            name: "job",
            status: HealthStatus::fail(format!("job {id} is stopped")),
        }]);
    }
    let allocs = nomad
        .get::<Vec<Value>>(&with_query(
            &format!("/v1/job/{}/allocations", seg(id)),
            "namespace",
            ns,
        ))
        .await?
        .unwrap_or_default();
    let deployment = nomad
        .get::<Value>(&with_query(
            &format!("/v1/job/{}/deployment", seg(id)),
            "namespace",
            ns,
        ))
        .await?
        .unwrap_or(Value::Null);

    let mut checks = vec![Check {
        name: "job",
        status: job_status(&job, &allocs, &deployment),
    }];
    if let Some(status) = services(nomad, ns, &allocs).await? {
        checks.push(Check {
            name: "services",
            status,
        });
    }
    Ok(checks)
}

/// The `job` check over already-fetched state.
pub fn job_status(job: &Value, allocs: &[Value], deployment: &Value) -> HealthStatus {
    let version = job["Version"].as_u64().unwrap_or(0);
    let current: Vec<&Value> = allocs
        .iter()
        .filter(|a| a["JobVersion"].as_u64() == Some(version))
        .filter(|a| a["DesiredStatus"].as_str() == Some("run"))
        .collect();
    let ty = job["Type"].as_str().unwrap_or("service");
    let failed: Vec<&str> = current
        .iter()
        .filter(|a| a["ClientStatus"].as_str() == Some("failed"))
        .filter_map(|a| a["TaskGroup"].as_str())
        .collect();

    if ty == "batch" || ty == "sysbatch" {
        if !failed.is_empty() {
            return HealthStatus::fail(format!(
                "{} failed allocation(s) in {}",
                failed.len(),
                failed.join(", ")
            ));
        }
        return HealthStatus::Pass;
    }

    let mut problems = Vec::new();
    for group in job["TaskGroups"].as_array().into_iter().flatten() {
        let name = group["Name"].as_str().unwrap_or_default();
        let mut by_status: BTreeMap<&str, u64> = BTreeMap::new();
        for a in current
            .iter()
            .filter(|a| a["TaskGroup"].as_str() == Some(name))
        {
            *by_status
                .entry(a["ClientStatus"].as_str().unwrap_or("unknown"))
                .or_default() += 1;
        }
        let running = by_status.get("running").copied().unwrap_or(0);
        let desired = match ty {
            // A system job runs one allocation per eligible node: whatever was placed.
            "system" => current
                .iter()
                .filter(|a| a["TaskGroup"].as_str() == Some(name))
                .count() as u64,
            _ => group["Count"].as_u64().unwrap_or(1),
        };
        if running < desired || (ty == "system" && desired == 0) {
            let others: Vec<String> = by_status
                .iter()
                .filter(|(s, _)| **s != "running")
                .map(|(s, n)| format!("{n} {s}"))
                .collect();
            let mut text = format!("group {name}: {running}/{desired} running");
            if !others.is_empty() {
                text.push_str(&format!(", {}", others.join(", ")));
            }
            problems.push(text);
        }
    }

    if deployment.is_object() && deployment["JobVersion"].as_u64() == Some(version) {
        match deployment["Status"].as_str().unwrap_or_default() {
            "successful" | "" => {}
            status => problems.push(format!(
                "deployment {status} ({})",
                deployment_progress(deployment)
            )),
        }
    }

    if problems.is_empty() {
        HealthStatus::Pass
    } else {
        HealthStatus::fail(problems.join("; "))
    }
}

/// The allocations whose service checks speak for the job: running and
/// meant to stay so. One Nomad is stopping (a destructive update's old
/// allocation through its `shutdown_delay`) still passes its checks.
fn serving(allocs: &[Value]) -> impl Iterator<Item = &Value> {
    allocs.iter().filter(|a| {
        a["ClientStatus"].as_str() == Some("running") && a["DesiredStatus"].as_str() == Some("run")
    })
}

/// The `services` check: Nomad-native service checks of serving allocations.
async fn services(nomad: &Nomad, ns: &str, allocs: &[Value]) -> Result<Option<HealthStatus>> {
    let mut total: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut detail: BTreeMap<String, String> = BTreeMap::new();
    for alloc in serving(allocs) {
        let id = alloc["ID"].as_str().unwrap_or_default();
        let url = with_query(
            &format!("/v1/allocation/{}/checks", seg(id)),
            "namespace",
            ns,
        );
        // Nomad answers `null` for an allocation without Nomad-native checks.
        let Some(checks) = nomad
            .get::<Option<BTreeMap<String, Value>>>(&url)
            .await?
            .flatten()
        else {
            continue;
        };
        for c in checks.values() {
            let service = c["Service"].as_str().unwrap_or("service").to_owned();
            let entry = total.entry(service.clone()).or_default();
            entry.0 += 1;
            if c["Status"].as_str() != Some("success") {
                entry.1 += 1;
                detail.entry(service).or_insert_with(|| {
                    format!(
                        "{} {}: {}",
                        c["Check"].as_str().unwrap_or("check"),
                        c["Status"].as_str().unwrap_or("unknown"),
                        c["Output"].as_str().unwrap_or("").trim()
                    )
                });
            }
        }
    }
    if total.is_empty() {
        return Ok(None);
    }
    let failing: Vec<String> = total
        .iter()
        .filter(|(_, (_, bad))| *bad > 0)
        .map(|(s, (all, bad))| {
            format!(
                "{s}: {bad} of {all} checks not passing ({})",
                detail.get(s).map(String::as_str).unwrap_or("")
            )
        })
        .collect();
    Ok(Some(if failing.is_empty() {
        HealthStatus::Pass
    } else {
        HealthStatus::fail(failing.join("; "))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn alloc(group: &str, version: u64, status: &str) -> Value {
        json!({"TaskGroup": group, "JobVersion": version, "DesiredStatus": "run", "ClientStatus": status})
    }

    fn service_job(count: u64) -> Value {
        json!({"Type": "service", "Version": 3, "TaskGroups": [{"Name": "api", "Count": count}]})
    }

    #[rstest::rstest]
    #[case::all_running(service_job(2), vec![alloc("api", 3, "running"), alloc("api", 3, "running")], json!(null), true)]
    #[case::one_pending(service_job(2), vec![alloc("api", 3, "running"), alloc("api", 3, "pending")], json!(null), false)]
    #[case::old_version_does_not_count(service_job(1), vec![alloc("api", 2, "running")], json!(null), false)]
    #[case::deployment_running(service_job(1), vec![alloc("api", 3, "running")], json!({"JobVersion": 3, "Status": "running", "TaskGroups": {"api": {"HealthyAllocs": 0, "DesiredTotal": 1}}}), false)]
    #[case::older_failed_deployment_ignored(service_job(1), vec![alloc("api", 3, "running")], json!({"JobVersion": 2, "Status": "failed"}), true)]
    #[case::batch_ok(json!({"Type": "batch", "Version": 0}), vec![alloc("run", 0, "complete")], json!(null), true)]
    #[case::batch_failed(json!({"Type": "batch", "Version": 0}), vec![alloc("run", 0, "failed")], json!(null), false)]
    #[case::system_unplaced(json!({"Type": "system", "Version": 0, "TaskGroups": [{"Name": "agent"}]}), vec![], json!(null), false)]
    fn job_health_follows_allocations_and_deployment(
        #[case] job: Value,
        #[case] allocs: Vec<Value>,
        #[case] deployment: Value,
        #[case] healthy: bool,
    ) {
        let status = job_status(&job, &allocs, &deployment);
        assert_eq!(status.is_pass(), healthy, "{status:?}");
    }

    #[rstest::rstest]
    #[case::running("run", "running", true)]
    #[case::stopping("stop", "running", false)]
    #[case::evicted("evict", "running", false)]
    #[case::pending("run", "pending", false)]
    fn only_serving_allocations_answer_for_services(
        #[case] desired: &str,
        #[case] client: &str,
        #[case] serves: bool,
    ) {
        let allocs = [json!({"DesiredStatus": desired, "ClientStatus": client})];
        assert_eq!(serving(&allocs).count() == 1, serves);
    }

    #[test]
    fn failures_say_what_is_missing() {
        let status = job_status(
            &service_job(2),
            &[alloc("api", 3, "running"), alloc("api", 3, "pending")],
            &Value::Null,
        );
        match status {
            HealthStatus::Fail { reason } => {
                assert!(reason.contains("api: 1/2 running, 1 pending"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }
}
