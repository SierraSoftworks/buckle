//! Nomad task `action` blocks, found in a workload's job specs when its
//! revision loads: each becomes one of the workload's implicit actions,
//! named after its block, with nothing written in `workload.yaml`.
//!
//! The spec is read as written (HCL2 variables and expressions are left
//! unevaluated); a spec that does not parse, such as a `.j2` template whose
//! directives are not HCL, contributes none.

use crate::model::WorkloadConfig;
use serde::{Deserialize, Serialize};

/// The kind a task action is offered and run as.
pub const KIND: &str = "nomad.action";

/// Which of a task group's allocations an action runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Instances {
    /// One running allocation, picked by Buckle.
    #[default]
    Any,
    /// Every running allocation, one after another.
    All,
}

impl Instances {
    pub fn as_str(self) -> &'static str {
        match self {
            Instances::Any => "any",
            Instances::All => "all",
        }
    }

    /// `any` or `all`; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "any" => Some(Instances::Any),
            "all" => Some(Instances::All),
            _ => None,
        }
    }
}

/// One task `action` block of a workload's jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAction {
    /// The workload action's name: the block's label, or `task.label` where
    /// two of the workload's tasks share the label or it is already an
    /// implicit or declared action's name.
    pub name: String,
    /// The job's id (its block label), group and task.
    pub job: String,
    pub group: String,
    pub task: String,
    /// The block's label, as Nomad knows the action.
    pub action: String,
    /// The command line, where `command` and `args` are written as plain
    /// strings.
    pub command: Option<String>,
    /// Where it runs unless the operator picks: every allocation of a
    /// system job's group, one of any other.
    pub instances: Instances,
    /// The group may run more than one allocation (a system job, or a
    /// `count` other than a literal 1), so the operator may pick.
    pub choice: bool,
}

impl TaskAction {
    /// What the action runs, for its description.
    pub fn description(&self) -> String {
        let place = format!("in task {} of job {}", self.task, self.job);
        match &self.command {
            Some(cmd) => format!("Runs `{cmd}` {place}."),
            None => format!("Runs Nomad action {} {place}.", self.action),
        }
    }
}

/// The `action` blocks of one job spec, unnamed (see [`name`]).
pub fn discover(hcl: &str) -> Vec<TaskAction> {
    let Ok(body) = hcl::parse(hcl) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for job in body.blocks().filter(|b| b.identifier() == "job") {
        let Some(job_id) = job.labels().first().map(|l| l.as_str().to_owned()) else {
            continue;
        };
        let system = matches!(
            literal(job.body(), "type").as_deref(),
            Some("system" | "sysbatch")
        );
        for group in job.body().blocks().filter(|b| b.identifier() == "group") {
            let Some(group_name) = group.labels().first().map(|l| l.as_str().to_owned()) else {
                continue;
            };
            let single = !system && count(group.body()) == Some(1);
            for task in group.body().blocks().filter(|b| b.identifier() == "task") {
                let Some(task_name) = task.labels().first().map(|l| l.as_str().to_owned()) else {
                    continue;
                };
                for action in task.body().blocks().filter(|b| b.identifier() == "action") {
                    let Some(label) = action.labels().first().map(|l| l.as_str().to_owned()) else {
                        continue;
                    };
                    out.push(TaskAction {
                        name: label.clone(),
                        job: job_id.clone(),
                        group: group_name.clone(),
                        task: task_name.clone(),
                        action: label,
                        command: command_line(action.body()),
                        instances: if system {
                            Instances::All
                        } else {
                            Instances::Any
                        },
                        choice: !single,
                    });
                }
            }
        }
    }
    out
}

/// Names `found` (every action of one workload's jobs, in spec order): the
/// label where it is unique and free, else `task.label`, else
/// `job.group.task.label`.
pub fn name(config: &WorkloadConfig, mut found: Vec<TaskAction>) -> Vec<TaskAction> {
    let taken = |n: &str| {
        config.implicit_actions().iter().any(|i| i.name == n) || config.actions.contains_key(n)
    };
    let labels: Vec<String> = found.iter().map(|a| a.action.clone()).collect();
    let tasks: Vec<String> = found
        .iter()
        .map(|a| format!("{}.{}", a.task, a.action))
        .collect();
    let count = |list: &[String], v: &str| list.iter().filter(|x| *x == v).count();
    for a in &mut found {
        let short = format!("{}.{}", a.task, a.action);
        a.name = if count(&labels, &a.action) == 1 && !taken(&a.action) {
            a.action.clone()
        } else if count(&tasks, &short) == 1 && !taken(&short) {
            short
        } else {
            format!("{}.{}.{}.{}", a.job, a.group, a.task, a.action)
        };
    }
    found
}

/// A plain string attribute.
fn literal(body: &hcl::Body, key: &str) -> Option<String> {
    match body.attributes().find(|a| a.key() == key)?.expr() {
        hcl::Expression::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// A group's `count` where written as a number (absent: Nomad's default, 1).
fn count(body: &hcl::Body) -> Option<u64> {
    match body.attributes().find(|a| a.key() == "count") {
        None => Some(1),
        Some(a) => match a.expr() {
            hcl::Expression::Number(n) => n.as_u64(),
            _ => None,
        },
    }
}

fn command_line(body: &hcl::Body) -> Option<String> {
    let mut words = vec![literal(body, "command")?];
    if let Some(args) = body.attributes().find(|a| a.key() == "args") {
        let hcl::Expression::Array(items) = args.expr() else {
            return None;
        };
        for item in items {
            let hcl::Expression::String(s) = item else {
                return None;
            };
            words.push(if s.is_empty() || s.contains(char::is_whitespace) {
                format!("{s:?}")
            } else {
                s.clone()
            });
        }
    }
    Some(words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
variable "version" { type = string }

job "nomad-auto-update" {
  type = "service"
  group "auto-update" {
    count = 1
    task "auto-update" {
      driver = "exec"
      action "check-now" {
        command = "/usr/bin/pkill"
        args    = ["-HUP", "-f", "nomad-auto-update -namespace="]
      }
      action "dry-run" {
        command = "local/nomad-auto-update"
        args    = ["-once", var.flag]
      }
    }
  }
}
"#;

    fn config(yaml: &str) -> WorkloadConfig {
        WorkloadConfig::parse(&format!("name: w\nresources: [w.nomad.hcl]\n{yaml}")).unwrap()
    }

    #[test]
    fn action_blocks_are_found_with_their_command() {
        let found = discover(SPEC);
        assert_eq!(
            found.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["check-now", "dry-run"]
        );
        let check = &found[0];
        assert_eq!(
            (
                check.job.as_str(),
                check.group.as_str(),
                check.task.as_str()
            ),
            ("nomad-auto-update", "auto-update", "auto-update")
        );
        assert_eq!(
            check.command.as_deref(),
            Some(r#"/usr/bin/pkill -HUP -f "nomad-auto-update -namespace=""#)
        );
        assert_eq!(found[1].command, None, "args hold an expression");
    }

    #[rstest::rstest]
    #[case::single("type = \"service\"", "count = 1", Instances::Any, false)]
    #[case::default_count("type = \"service\"", "", Instances::Any, false)]
    #[case::several("type = \"service\"", "count = 3", Instances::Any, true)]
    #[case::variable_count("", "count = var.n", Instances::Any, true)]
    #[case::system("type = \"system\"", "", Instances::All, true)]
    fn where_an_action_runs_follows_the_job_type_and_count(
        #[case] ty: &str,
        #[case] count: &str,
        #[case] instances: Instances,
        #[case] choice: bool,
    ) {
        let spec = format!(
            "job \"j\" {{\n{ty}\ngroup \"g\" {{\n{count}\ntask \"t\" {{\naction \"a\" {{ command = \"/bin/true\" }}\n}}\n}}\n}}\n"
        );
        let found = discover(&spec);
        assert_eq!(
            (found[0].instances, found[0].choice),
            (instances, choice),
            "{spec}"
        );
    }

    #[test]
    fn a_spec_that_is_not_hcl_has_no_actions() {
        assert!(discover("job \"x\" { {% if a %} }").is_empty());
    }

    #[test]
    fn names_are_qualified_only_where_they_collide() {
        let spec = r#"
job "j" {
  group "g" {
    task "a" {
      action "reload" { command = "x" }
      action "restart" { command = "x" }
      action "backfill" { command = "x" }
    }
    task "b" {
      action "reload" { command = "x" }
      action "stats" { command = "x" }
    }
  }
}
"#;
        let cfg = config("actions:\n  backfill: { dispatch: { job: j } }\n");
        let named = name(&cfg, discover(spec));
        assert_eq!(
            named.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["a.reload", "a.restart", "a.backfill", "b.reload", "stats"]
        );
    }
}
