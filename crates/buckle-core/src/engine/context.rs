//! Building the host context: merged labels, derived groups, and vars with
//! the precedence of design §03 (module defaults < fleet overlays in order <
//! agent-local overrides), rendered lazily with cycle detection.

use super::Revision;
use crate::model::ModuleConfig;
use crate::selectors::{HostContext, Labels, Vars};
use crate::template::Templater;
use indexmap::IndexMap;
use std::collections::BTreeSet;

/// The agent's labels with the server's overlaid; the server wins on conflicts.
pub fn merged_labels(agent: &Labels, server: &Labels) -> Labels {
    let mut out = agent.clone();
    for (k, v) in server {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Evaluates the fleet's group selectors against `ctx` (labels and facts; no
/// groups yet). Groups keep their declaration order.
pub fn evaluate_groups(revision: &Revision, ctx: &HostContext) -> Vec<String> {
    revision
        .fleet
        .fleet
        .groups
        .iter()
        .filter(|(_, selector)| ctx.matches(selector))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Converts a YAML value into JSON. Non-string mapping keys are stringified.
pub fn yaml_to_json(value: &serde_yaml::Value) -> serde_json::Value {
    match value {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::from(i)
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::from(u)
            } else {
                serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0))
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(items) => {
            serde_json::Value::Array(items.iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (yaml_key(k), yaml_to_json(v)))
                .collect(),
        ),
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(&tagged.value),
    }
}

fn yaml_key(key: &serde_yaml::Value) -> String {
    match key {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => "null".to_owned(),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default(),
    }
}

/// Renders every string scalar in `value` that contains template syntax with
/// [`Templater::render_plain`] and returns the result as JSON. A reference to a
/// secret is an error: task spec fields are plain strings that end up in plans.
pub fn render_value(
    value: &serde_yaml::Value,
    templater: &Templater,
    ctx: &HostContext,
) -> crate::Result<serde_json::Value> {
    match value {
        serde_yaml::Value::String(s) if Templater::needs_render(s) => {
            Ok(serde_json::Value::String(templater.render_plain(s, ctx)?))
        }
        serde_yaml::Value::Sequence(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|v| render_value(v, templater, ctx))
                .collect::<crate::Result<Vec<_>>>()?,
        )),
        serde_yaml::Value::Mapping(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(yaml_key(k), render_value(v, templater, ctx)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        serde_yaml::Value::Tagged(tagged) => render_value(&tagged.value, templater, ctx),
        other => Ok(yaml_to_json(other)),
    }
}

/// Collects the template strings inside a JSON value.
fn template_strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(s) if Templater::needs_render(s) => out.push(s),
        serde_json::Value::Array(items) => items.iter().for_each(|v| template_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| template_strings(v, out)),
        _ => {}
    }
}

/// Layers the raw (unrendered) vars for a module: module defaults, then each
/// fleet overlay whose `when` matches (an overlay without `when` always
/// applies), then the agent-local overrides.
pub fn layer_vars(
    revision: &Revision,
    module: &ModuleConfig,
    agent_vars: &Vars,
    ctx: &HostContext,
) -> crate::Result<Vars> {
    let mut vars: Vars = module
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), yaml_to_json(v)))
        .collect();
    for (index, overlay) in revision.fleet.vars.iter().enumerate() {
        if let Some(when) = &overlay.when {
            let scoped = ctx.clone().with_vars(vars.clone());
            if !scoped.matches(when) {
                continue;
            }
        }
        if let Some(file) = &overlay.file {
            let content = revision.var_files.get(file).ok_or_else(|| {
                crate::error::system(
                    format!(
                        "The vars file {} (overlay {}) was not loaded with the revision.",
                        file.display(),
                        index + 1
                    ),
                    &["This is a bug in the revision loader; please report it."],
                )
            })?;
            match content {
                serde_yaml::Value::Mapping(map) => {
                    for (k, v) in map {
                        vars.insert(yaml_key(k), yaml_to_json(v));
                    }
                }
                serde_yaml::Value::Null => {}
                _ => {
                    return Err(crate::error::user(
                        format!(
                            "The vars file {} must contain a mapping of variable names to values.",
                            file.display()
                        ),
                        &["Write each variable as `name: value` at the top level of the file."],
                    ));
                }
            }
        }
        for (k, v) in &overlay.values {
            vars.insert(k.clone(), yaml_to_json(v));
        }
    }
    for (k, v) in agent_vars {
        vars.insert(k.clone(), v.clone());
    }
    Ok(vars)
}

/// Renders the template strings inside `raw` lazily: a var is rendered after the
/// vars it references, and a reference cycle is a user error naming the cycle.
pub fn render_vars(
    raw: Vars,
    module: &str,
    templater: &Templater,
    ctx: &HostContext,
) -> crate::Result<Vars> {
    let mut resolver = VarResolver {
        raw: &raw,
        resolved: IndexMap::new(),
        module,
        templater,
        ctx,
    };
    for name in raw.keys() {
        resolver.resolve(name, &mut Vec::new())?;
    }
    // Keep the declaration order of the layered map.
    let mut out = Vars::new();
    for name in raw.keys() {
        if let Some(v) = resolver.resolved.get(name) {
            out.insert(name.clone(), v.clone());
        }
    }
    Ok(out)
}

struct VarResolver<'a> {
    raw: &'a Vars,
    resolved: IndexMap<String, serde_json::Value>,
    module: &'a str,
    templater: &'a Templater,
    ctx: &'a HostContext,
}

impl VarResolver<'_> {
    fn resolve(&mut self, name: &str, stack: &mut Vec<String>) -> crate::Result<()> {
        if self.resolved.contains_key(name) {
            return Ok(());
        }
        if let Some(pos) = stack.iter().position(|s| s == name) {
            let mut cycle: Vec<&str> = stack[pos..].iter().map(String::as_str).collect();
            cycle.push(name);
            return Err(crate::error::user(
                format!(
                    "The vars of module '{}' reference each other in a cycle: {}.",
                    self.module,
                    cycle.join(" -> ")
                ),
                &["Break the cycle by giving one of the variables a literal value."],
            ));
        }
        let Some(value) = self.raw.get(name) else {
            return Ok(());
        };
        let mut templates = Vec::new();
        template_strings(value, &mut templates);
        if templates.is_empty() {
            self.resolved.insert(name.to_owned(), value.clone());
            return Ok(());
        }
        stack.push(name.to_owned());
        let mut deps = BTreeSet::new();
        for source in &templates {
            for path in self.templater.referenced_paths(source).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!(
                        "The var '{name}' of module '{}' is not a valid template.",
                        self.module
                    ),
                    &["Fix the template syntax in the var's value."],
                )
            })? {
                if let Some(rest) = path.strip_prefix("vars.") {
                    let dep = rest.split('.').next().unwrap_or(rest);
                    if !dep.is_empty() {
                        deps.insert(dep.to_owned());
                    }
                }
            }
        }
        for dep in &deps {
            if self.raw.contains_key(dep) {
                self.resolve(dep, stack)?;
            } else {
                return Err(crate::error::user(
                    format!(
                        "The var '{name}' of module '{}' references 'vars.{dep}', which is not defined.",
                        self.module
                    ),
                    &[
                        "Define the variable in the module's vars, a fleet overlay or the agent's vars.",
                        "Use `{{ vars.name | default('x') }}` when the variable is optional.",
                    ],
                ));
            }
        }
        stack.pop();
        // Render with the dependencies resolved; unrelated vars stay raw, which
        // is harmless because the template does not read them.
        let mut vars = self.raw.clone();
        for (k, v) in &self.resolved {
            vars.insert(k.clone(), v.clone());
        }
        let scoped = self.ctx.clone().with_vars(vars);
        let rendered = render_json(value, self.templater, &scoped).map_err(|e| {
            crate::error::wrap_user(
                e,
                format!(
                    "The var '{name}' of module '{}' could not be rendered.",
                    self.module
                ),
                &[
                    "Vars may reference facts, labels, groups and other vars, but not secrets: use a secret directly in the file template or task field that needs it.",
                ],
            )
        })?;
        self.resolved.insert(name.to_owned(), rendered);
        Ok(())
    }
}

fn render_json(
    value: &serde_json::Value,
    templater: &Templater,
    ctx: &HostContext,
) -> crate::Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) if Templater::needs_render(s) => {
            Ok(serde_json::Value::String(templater.render_plain(s, ctx)?))
        }
        serde_json::Value::Array(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|v| render_json(v, templater, ctx))
                .collect::<crate::Result<Vec<_>>>()?,
        )),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), render_json(v, templater, ctx)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// Layers and renders the vars for one module.
pub fn resolve_vars(
    revision: &Revision,
    module: &ModuleConfig,
    agent_vars: &Vars,
    templater: &Templater,
    ctx: &HostContext,
) -> crate::Result<Vars> {
    let raw = layer_vars(revision, module, agent_vars, ctx)?;
    render_vars(raw, &module.name, templater, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("a: 1", serde_json::json!({"a": 1}))]
    #[case("a: [1, two, true]", serde_json::json!({"a": [1, "two", true]}))]
    #[case("1: x\ntrue: y", serde_json::json!({"1": "x", "true": "y"}))]
    #[case("a: 1.5", serde_json::json!({"a": 1.5}))]
    #[case("a: ~", serde_json::json!({"a": null}))]
    fn yaml_converts_to_json(#[case] yaml: &str, #[case] expected: serde_json::Value) {
        let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(yaml_to_json(&v), expected);
    }

    #[test]
    fn server_labels_override_agent_labels() {
        let mut agent = Labels::new();
        agent.insert("role".into(), "worker".into());
        agent.insert("site".into(), "home".into());
        let mut server = Labels::new();
        server.insert("role".into(), "server".into());
        let merged = merged_labels(&agent, &server);
        assert_eq!(merged["role"], "server");
        assert_eq!(merged["site"], "home");
    }
}
