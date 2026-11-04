//! Rendering what will change: field-level diffs of entities Buckle compares
//! itself, and Nomad's own structured job plan diff and scheduler preview.

use serde_json::Value;

/// Whether every field `desired` sets has the same value in `live`. Fields
/// the API adds (indexes, defaults, hashes) are ignored; maps under a field
/// listed in `exact` (user maps such as `Meta`) must match key for key.
pub fn subset(desired: &Value, live: &Value, exact: &[&str]) -> bool {
    field_changes(desired, live, exact).is_empty()
}

/// The fields of `desired` that differ from `live`, as `path: old => new` lines.
pub fn field_changes(desired: &Value, live: &Value, exact: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    walk("", desired, live, exact, &mut out);
    out
}

fn walk(path: &str, desired: &Value, live: &Value, exact: &[&str], out: &mut Vec<String>) {
    match (desired, live) {
        (Value::Object(d), Value::Object(l)) => {
            let name = path.rsplit('.').next().unwrap_or(path);
            let is_exact = exact.contains(&name);
            for (k, dv) in d {
                let p = join(path, k);
                match l.get(k) {
                    Some(lv) if is_exact => {
                        if dv != lv {
                            out.push(format!("~ {p}: {} => {}", short(lv), short(dv)));
                        }
                    }
                    Some(lv) => walk(&p, dv, lv, exact, out),
                    None if is_empty(dv) => {}
                    None => out.push(format!("+ {p}: {}", short(dv))),
                }
            }
            if is_exact {
                for (k, lv) in l {
                    if !d.contains_key(k) {
                        out.push(format!("- {}: {}", join(path, k), short(lv)));
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(l)) if d.len() == l.len() => {
            for (i, (dv, lv)) in d.iter().zip(l).enumerate() {
                walk(&format!("{path}[{i}]"), dv, lv, exact, out);
            }
        }
        (d, Value::Null) if is_empty(d) => {}
        (d, l) if d == l => {}
        (Value::Number(d), Value::Number(l)) if d.as_f64() == l.as_f64() => {}
        (d, l) => out.push(format!("~ {path}: {} => {}", short(l), short(d))),
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s
    }
}

/// Renders Nomad's structured job diff (`/v1/job/:id/plan` with `Diff`) the
/// way `nomad job plan` prints it, skipping unchanged fields.
pub fn render_job_diff(diff: &Value) -> String {
    let mut out = String::new();
    let ty = diff["Type"].as_str().unwrap_or("None");
    out.push_str(&format!("{} Job: {}\n", marker(ty), diff["ID"]));
    fields(diff, 1, &mut out);
    objects(diff, 1, &mut out);
    for tg in diff["TaskGroups"].as_array().into_iter().flatten() {
        let ty = tg["Type"].as_str().unwrap_or("None");
        let updates = tg["Updates"]
            .as_object()
            .map(|u| {
                u.iter()
                    .map(|(k, n)| format!("{n} {k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        if ty == "None" && updates.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{}{} Task Group: {}{updates}\n",
            indent(1),
            marker(ty),
            tg["Name"]
        ));
        fields(tg, 2, &mut out);
        objects(tg, 2, &mut out);
        for task in tg["Tasks"].as_array().into_iter().flatten() {
            let ty = task["Type"].as_str().unwrap_or("None");
            if ty == "None" {
                continue;
            }
            let notes = task["Annotations"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}{} Task: {}{notes}\n",
                indent(2),
                marker(ty),
                task["Name"]
            ));
            fields(task, 3, &mut out);
            objects(task, 3, &mut out);
        }
    }
    out
}

fn fields(node: &Value, depth: usize, out: &mut String) {
    for f in node["Fields"].as_array().into_iter().flatten() {
        let ty = f["Type"].as_str().unwrap_or("None");
        if ty == "None" {
            continue;
        }
        let name = f["Name"].as_str().unwrap_or("");
        let (old, new) = (
            f["Old"].as_str().unwrap_or(""),
            f["New"].as_str().unwrap_or(""),
        );
        let value = match ty {
            "Added" => format!("{new:?}"),
            "Deleted" => format!("{old:?}"),
            _ => format!("{old:?} => {new:?}"),
        };
        out.push_str(&format!(
            "{}{} {name}: {value}\n",
            indent(depth),
            marker(ty)
        ));
    }
}

fn objects(node: &Value, depth: usize, out: &mut String) {
    for o in node["Objects"].as_array().into_iter().flatten() {
        let ty = o["Type"].as_str().unwrap_or("None");
        if ty == "None" {
            continue;
        }
        let name = o["Name"].as_str().unwrap_or("");
        out.push_str(&format!("{}{} {name} {{\n", indent(depth), marker(ty)));
        fields(o, depth + 1, out);
        objects(o, depth + 1, out);
        out.push_str(&format!("{}}}\n", indent(depth + 1)));
    }
}

fn marker(ty: &str) -> &'static str {
    match ty {
        "Added" => "+",
        "Deleted" => "-",
        "Edited" => "+/-",
        _ => " ",
    }
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// The scheduler preview of a job plan, one line per group:
/// `api: 1 create/destroy update, 1 ignore`.
pub fn scheduler_summary(plan: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(groups) = plan["Annotations"]["DesiredTGUpdates"].as_object() else {
        return lines;
    };
    for (group, u) in groups {
        let parts: Vec<String> = [
            ("Place", "create"),
            ("DestructiveUpdate", "create/destroy update"),
            ("InPlaceUpdate", "in-place update"),
            ("Canary", "canary"),
            ("Migrate", "migrate"),
            ("Stop", "stop"),
            ("Preemptions", "preempt"),
            ("Ignore", "ignore"),
        ]
        .iter()
        .filter_map(|(k, label)| {
            let n = u[*k].as_u64().unwrap_or(0);
            (n > 0).then(|| format!("{n} {label}"))
        })
        .collect();
        if !parts.is_empty() {
            lines.push(format!("{group}: {}", parts.join(", ")));
        }
    }
    lines
}

/// Placement failures and scheduler warnings of a job plan, as plan warnings.
pub fn plan_warnings(plan: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(failed) = plan["FailedTGAllocs"].as_object() {
        for (group, metric) in failed {
            out.push(format!("{group}: {}", placement_failure(metric)));
        }
    }
    if let Some(w) = plan["Warnings"].as_str().filter(|w| !w.trim().is_empty()) {
        out.extend(
            w.lines()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty()),
        );
    }
    out
}

/// One group's placement failure: `cannot place 1 allocation (3 nodes
/// evaluated; constraint ${attr.kernel.name} = linux filtered 3)`.
pub fn placement_failure(metric: &Value) -> String {
    let mut reasons = Vec::new();
    for key in ["ConstraintFiltered", "DimensionExhausted", "ClassFiltered"] {
        if let Some(m) = metric[key].as_object() {
            for (why, n) in m {
                reasons.push(format!("{why} filtered {n}"));
            }
        }
    }
    let evaluated = metric["NodesEvaluated"].as_u64().unwrap_or(0);
    let mut text = format!(
        "cannot place {} allocation(s) ({evaluated} nodes evaluated",
        metric["CoalescedFailures"].as_u64().unwrap_or(0) + 1
    );
    if metric["NodesAvailable"]
        .as_object()
        .is_some_and(|m| m.values().all(|n| n == 0))
    {
        text.push_str("; no nodes available in the datacenters");
    }
    for r in reasons {
        text.push_str("; ");
        text.push_str(&r);
    }
    text.push(')');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[rstest::rstest]
    #[case::equal(json!({"Name": "a", "Description": "x"}), json!({"Name": "a", "Description": "x", "ModifyIndex": 4}), 0)]
    #[case::changed(json!({"Description": "y"}), json!({"Description": "x"}), 1)]
    #[case::empty_desired_matches_absent(json!({"Meta": {}, "Quota": ""}), json!({"Quota": null}), 0)]
    #[case::exact_maps_see_removed_keys(json!({"Meta": {"a": "1"}}), json!({"Meta": {"a": "1", "b": "2"}}), 1)]
    #[case::nested(json!({"Capabilities": {"EnabledTaskDrivers": ["docker"]}}), json!({"Capabilities": {"EnabledTaskDrivers": ["docker", "exec"]}}), 1)]
    fn entity_fields_compare_on_what_the_spec_sets(
        #[case] desired: Value,
        #[case] live: Value,
        #[case] changes: usize,
    ) {
        assert_eq!(field_changes(&desired, &live, &["Meta"]).len(), changes);
    }

    #[test]
    fn job_diffs_render_changed_fields_only() {
        let diff = json!({
            "Type": "Edited", "ID": "paste",
            "Fields": [{"Type": "None", "Name": "Priority", "Old": "50", "New": "50"}],
            "TaskGroups": [{
                "Type": "Edited", "Name": "api", "Updates": {"create/destroy update": 1},
                "Tasks": [{
                    "Type": "Edited", "Name": "paste", "Annotations": ["forces create/destroy update"],
                    "Objects": [{"Type": "Edited", "Name": "Config", "Fields": [
                        {"Type": "Edited", "Name": "image", "Old": "tclip:1", "New": "tclip:2"}
                    ]}]
                }]
            }]
        });
        let text = render_job_diff(&diff);
        assert!(text.contains("+/- Job: \"paste\""), "{text}");
        assert!(
            text.contains("Task Group: \"api\" (1 create/destroy update)"),
            "{text}"
        );
        assert!(text.contains("image: \"tclip:1\" => \"tclip:2\""), "{text}");
        assert!(!text.contains("Priority"), "{text}");
    }

    #[test]
    fn the_scheduler_preview_and_failures_are_summarised() {
        let plan = json!({
            "Annotations": {"DesiredTGUpdates": {"api": {"DestructiveUpdate": 1, "Ignore": 1, "Place": 0}}},
            "FailedTGAllocs": {"api": {"NodesEvaluated": 3, "ConstraintFiltered": {"missing host network \"tailscale\"": 3}}},
            "Warnings": "Group \"api\" has warnings\n"
        });
        assert_eq!(
            scheduler_summary(&plan),
            vec!["api: 1 create/destroy update, 1 ignore"]
        );
        let warnings = plan_warnings(&plan);
        assert!(
            warnings[0].contains("3 nodes evaluated") && warnings[0].contains("tailscale"),
            "{warnings:?}"
        );
        assert_eq!(warnings.len(), 2);
    }
}
