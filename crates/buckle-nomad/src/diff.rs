//! Rendering what will change: field-level diffs of entities Buckle compares
//! itself, and Nomad's own structured job plan diff and scheduler preview.

use serde_json::Value;

/// Whether every field `desired` sets has the same value in `live`. Fields
/// the API adds (indexes, defaults, hashes) are ignored; maps under a field
/// listed in `exact` (user maps such as `Meta`) must match key for key.
pub fn subset(desired: &Value, live: &Value, exact: &[&str]) -> bool {
    field_changes(desired, live, exact).is_empty()
}

/// The fields of `desired` that differ from `live`, one unified diff entry
/// each: `+ path: new`, `- path: old`, or a `-`/`+` pair for an edit.
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
                            out.push(edited(&p, lv, dv));
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
        (d, l) => out.push(edited(path, l, d)),
    }
}

/// One changed field, as the `-`/`+` pair of a unified diff.
fn edited(path: &str, live: &Value, desired: &Value) -> String {
    format!("- {path}: {}\n+ {path}: {}", short(live), short(desired))
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

/// Renders Nomad's structured job diff (`/v1/job/:id/plan` with `Diff`) as a
/// unified diff, so it reads like Buckle's other diffs: the first column is
/// `+`/`-` for added and removed lines (an edited field is a `-`/`+` pair) and
/// blank for the job, groups, tasks and blocks that hold them; indentation
/// after it carries the nesting. Unchanged fields are skipped.
pub fn render_job_diff(diff: &Value) -> String {
    let mut out = Vec::new();
    let notes = |n: &Value, key: &str| match &n[key] {
        Value::Object(u) => u
            .iter()
            .map(|(k, n)| format!("{n} {k}"))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    };
    let head = |n: &Value, label: &str, key: &str| {
        let notes = notes(n, key);
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({notes})")
        };
        format!("{label} {}{notes}", n["Name"])
    };

    let ty = kind(diff);
    out.push(line(ty, 0, &format!("Job {}", diff["ID"])));
    body(diff, ty, 1, &mut out);
    for tg in diff["TaskGroups"].as_array().into_iter().flatten() {
        let ty = kind(tg);
        if ty == "None" && notes(tg, "Updates").is_empty() {
            continue;
        }
        out.push(line(ty, 1, &head(tg, "Task Group", "Updates")));
        body(tg, ty, 2, &mut out);
        for task in tg["Tasks"].as_array().into_iter().flatten() {
            let ty = kind(task);
            if ty == "None" {
                continue;
            }
            out.push(line(ty, 2, &head(task, "Task", "Annotations")));
            body(task, ty, 3, &mut out);
        }
    }
    out.join("\n") + "\n"
}

fn kind(node: &Value) -> &str {
    node["Type"].as_str().unwrap_or("None")
}

/// The changed fields and blocks of a node whose own change is `parent`.
fn body(node: &Value, parent: &str, depth: usize, out: &mut Vec<String>) {
    for f in node["Fields"].as_array().into_iter().flatten() {
        field(f, depth, out);
    }
    for o in node["Objects"].as_array().into_iter().flatten() {
        let ty = kind(o);
        if ty == "None" {
            continue;
        }
        // A block inside an added or removed node is added or removed with it.
        let ty = if ty == "Edited" && parent != "Edited" {
            parent
        } else {
            ty
        };
        let name = o["Name"].as_str().unwrap_or("");
        out.push(line(ty, depth, &format!("{name} {{")));
        body(o, ty, depth + 1, out);
        out.push(line(ty, depth, "}"));
    }
}

fn field(f: &Value, depth: usize, out: &mut Vec<String>) {
    let ty = kind(f);
    if ty == "None" {
        return;
    }
    let name = f["Name"].as_str().unwrap_or("");
    let (old, new) = (
        f["Old"].as_str().unwrap_or(""),
        f["New"].as_str().unwrap_or(""),
    );
    if old.contains('\n') || new.contains('\n') {
        // Templates and other text blocks: diff them line by line.
        let (old, new) = match ty {
            "Added" => ("", new),
            "Deleted" => (old, ""),
            _ => (old, new),
        };
        out.push(line(ty, depth, &format!("{name}: <<EOF")));
        let text = similar::TextDiff::from_lines(old, new);
        // Three lines of context around each change, as in other unified diffs.
        for (i, group) in text.grouped_ops(3).iter().enumerate() {
            if i > 0 {
                out.push(line("Edited", depth + 1, "…"));
            }
            for change in group.iter().flat_map(|op| text.iter_changes(op)) {
                let ty = match change.tag() {
                    similar::ChangeTag::Insert => "Added",
                    similar::ChangeTag::Delete => "Deleted",
                    similar::ChangeTag::Equal => "Edited",
                };
                out.push(line(
                    ty,
                    depth + 1,
                    change.value().trim_end_matches(['\r', '\n']),
                ));
            }
        }
        out.push(line(ty, depth, "EOF"));
        return;
    }
    match ty {
        "Added" => out.push(line(ty, depth, &format!("{name}: {new:?}"))),
        "Deleted" => out.push(line(ty, depth, &format!("{name}: {old:?}"))),
        _ => {
            out.push(line("Deleted", depth, &format!("{name}: {old:?}")));
            out.push(line("Added", depth, &format!("{name}: {new:?}")));
        }
    }
}

/// One unified diff line: the change marker, then two spaces per level.
fn line(ty: &str, depth: usize, text: &str) -> String {
    let marker = match ty {
        "Added" => '+',
        "Deleted" => '-',
        _ => ' ',
    };
    format!("{marker}{}{text}", "  ".repeat(depth))
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

    /// The `bender` plan from Ben's screenshot: meta keys added and removed,
    /// an edited image inside an edited task, and an untouched field.
    fn edited_job() -> Value {
        json!({
            "Type": "Edited", "ID": "bender",
            "Fields": [
                {"Type": "None", "Name": "Priority", "Old": "50", "New": "50"},
                {"Type": "Added", "Name": "Meta[buckle.module]", "Old": "", "New": "bender"},
                {"Type": "Deleted", "Name": "Meta[pack.name]", "Old": "bender", "New": ""}
            ],
            "TaskGroups": [{
                "Type": "Edited", "Name": "api", "Updates": {"create/destroy update": 1, "ignore": 1},
                "Tasks": [{
                    "Type": "Edited", "Name": "bender", "Annotations": ["forces create/destroy update"],
                    "Objects": [{"Type": "Edited", "Name": "Config", "Fields": [
                        {"Type": "Edited", "Name": "image", "Old": "bender:main@sha256:213a", "New": "bender:main"}
                    ]}, {"Type": "Added", "Name": "Template", "Fields": [
                        {"Type": "Added", "Name": "EmbeddedTmpl", "Old": "", "New": "a = 1\nb = 2\n"}
                    ]}]
                }]
            }]
        })
    }

    /// The lines of a unified diff with the given marker, marker stripped and trimmed.
    fn marked(text: &str, marker: char) -> Vec<&str> {
        text.lines()
            .filter_map(|l| l.strip_prefix(marker))
            .map(str::trim)
            .collect()
    }

    #[test]
    fn job_diffs_render_as_unified_diffs() {
        let text = render_job_diff(&edited_job());
        assert!(
            text.lines()
                .all(|l| l.starts_with(['+', '-', ' ']) && !l.starts_with("+/-")),
            "{text}"
        );
        let (added, removed) = (marked(&text, '+'), marked(&text, '-'));
        assert!(added.contains(&"Meta[buckle.module]: \"bender\""), "{text}");
        assert!(removed.contains(&"Meta[pack.name]: \"bender\""), "{text}");
        assert!(
            removed.contains(&"image: \"bender:main@sha256:213a\""),
            "{text}"
        );
        assert!(added.contains(&"image: \"bender:main\""), "{text}");
        // An added block's text is added line by line, braces included.
        for l in ["Template {", "a = 1", "b = 2", "}"] {
            assert!(added.contains(&l), "{l} in {text}");
        }
        assert!(!text.contains("Priority"), "{text}");
        // Containers are context, nested by indentation.
        let context = marked(&text, ' ');
        assert!(
            context.contains(&"Task Group \"api\" (1 create/destroy update, 1 ignore)"),
            "{text}"
        );
        let indent = |needle: &str| {
            let l = text.lines().find(|l| l.contains(needle)).unwrap();
            l.len() - l[1..].trim_start().len()
        };
        assert!(
            indent("Job ") < indent("Task Group")
                && indent("Task Group") < indent("Task \"")
                && indent("Task \"") < indent("image:"),
            "{text}"
        );
    }

    #[test]
    fn edited_text_fields_diff_line_by_line() {
        let diff = json!({"Type": "Edited", "ID": "j", "Fields": [
            {"Type": "Edited", "Name": "Tmpl", "Old": "a\nb\nc\n", "New": "a\nB\nc\n"}
        ]});
        let text = render_job_diff(&diff);
        assert_eq!(marked(&text, '-'), vec!["b"], "{text}");
        assert_eq!(marked(&text, '+'), vec!["B"], "{text}");
        assert!(marked(&text, ' ').contains(&"a"), "context kept: {text}");
    }

    #[test]
    fn edited_entity_fields_are_removed_then_added() {
        let changes = field_changes(
            &json!({"Description": "y"}),
            &json!({"Description": "x"}),
            &[],
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(marked(&changes[0], '-'), vec!["Description: \"x\""]);
        assert_eq!(marked(&changes[0], '+'), vec!["Description: \"y\""]);
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
