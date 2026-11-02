//! Behavioural checks on the reference fleet in `examples/nomad-cluster`.
//!
//! The example is documentation that must keep working, so these tests assert
//! what an operator would care about: it validates, every kind it names is
//! registered, the module graph is sound, the vars overlays give each host the
//! values the precedence rules promise, every task spec parses into its typed
//! form and every template renders, for each of the seven fictional hosts.
//!
//! The hosts are described by the facts fixtures in `fixtures/facts/` plus the
//! labels an agent would report. Vars are resolved here with the documented
//! rules (design section 03: module defaults, then fleet overlays in list
//! order) using only `buckle-core` primitives, so the tests stand on the public
//! model rather than on any one engine entry point.

use buckle_core::engine::loader::load_dir;
use buckle_core::engine::{LoadedModule, Revision};
use buckle_core::model::{ApprovalPolicy, TaskDecl};
use buckle_core::plan::RevisionId;
use buckle_core::secrets::field::render_spec_strings;
use buckle_core::secrets::{InstallKey, Secret, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity, Labels, Vars};
use buckle_core::task::resource_key;
use buckle_core::template::Templater;
use rstest::{fixture, rstest};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn example_dir() -> PathBuf {
    repo_root().join("examples/nomad-cluster")
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("configuration values convert to JSON")
}

/// The seven fictional hosts: id, facts fixture and the labels their agents report.
fn host_description(id: &str) -> (&'static str, Value) {
    match id {
        "pi-1" | "pi-2" | "pi-3" => (
            "debian-arm64",
            json!({"role": "worker", "site": "home", "nomad_server": true}),
        ),
        "pi-4" => (
            "debian-arm64",
            json!({"role": "worker", "site": "home", "canary": true}),
        ),
        "oci-1" | "oci-2" => ("ubuntu-arm64", json!({"role": "worker", "site": "oci"})),
        "az-1" => ("ubuntu-amd64", json!({"role": "worker", "site": "azure"})),
        other => panic!("the example has no host called {other}"),
    }
}

/// The loaded example plus what it takes to evaluate it for a host.
struct Example {
    revision: Revision,
    templater: Templater,
}

#[fixture]
fn example() -> Example {
    buckle_std::ensure_linked();
    let revision = load_dir(&example_dir(), RevisionId::new("example")).expect("the example loads");
    Example {
        revision,
        templater: Templater::new(),
    }
}

/// A module that is selected on a host, with the context its tasks see.
struct Selected<'a> {
    module: &'a LoadedModule,
    ctx: HostContext,
}

impl Example {
    fn module(&self, name: &str) -> &LoadedModule {
        self.revision
            .modules
            .iter()
            .find(|m| m.config.name == name)
            .unwrap_or_else(|| panic!("the example has no module called {name}"))
    }

    fn secrets(&self) -> Arc<SecretStore> {
        let key = InstallKey::from_bytes([0u8; 32]);
        let mut store = SecretStore::new();
        for name in self.revision.fleet.secrets.keys() {
            store.insert(Secret::new(name.clone(), "placeholder".into(), &key));
        }
        Arc::new(store)
    }

    /// Labels, facts, groups and placeholder secrets for a host; no module scope yet.
    fn host(&self, id: &str) -> HostContext {
        let (fixture, labels) = host_description(id);
        let facts_path = repo_root().join(format!("fixtures/facts/{fixture}.json"));
        let facts: Value = serde_json::from_str(
            &std::fs::read_to_string(&facts_path).expect("the facts fixture exists"),
        )
        .expect("the facts fixture is JSON");
        let labels: Labels = serde_json::from_value(labels).expect("labels are a flat map");
        let base = HostContext::new(HostIdentity {
            id: id.to_owned(),
            labels,
        })
        .with_facts(facts)
        .with_secrets(self.secrets());
        let groups = self
            .revision
            .fleet
            .fleet
            .groups
            .iter()
            .filter(|(_, selector)| base.matches(selector))
            .map(|(name, _)| name.clone())
            .collect();
        base.with_groups(groups)
    }

    /// Module defaults, then every matching fleet overlay in list order, with
    /// templated values rendered (a var may use facts and other vars).
    fn vars_for(&self, module: &LoadedModule, scoped: &HostContext) -> Vars {
        let mut vars: Vars = module
            .config
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), to_json(v)))
            .collect();
        for overlay in &self.revision.fleet.vars {
            if let Some(when) = &overlay.when
                && !scoped.clone().with_vars(vars.clone()).matches(when)
            {
                continue;
            }
            if let Some(file) = &overlay.file {
                let content = to_json(&self.revision.var_files[file]);
                for (k, v) in content.as_object().expect("a vars file is a mapping") {
                    vars.insert(k.clone(), v.clone());
                }
            }
            for (k, v) in &overlay.values {
                vars.insert(k.clone(), to_json(v));
            }
        }
        for _ in 0..8 {
            let ctx = scoped.clone().with_vars(vars.clone());
            let next: Vars = vars
                .iter()
                .map(|(k, v)| (k.clone(), self.render_value(v, &ctx)))
                .collect();
            if next == vars {
                break;
            }
            vars = next;
        }
        vars
    }

    fn render_value(&self, value: &Value, ctx: &HostContext) -> Value {
        match value {
            Value::String(s) if Templater::needs_render(s) => Value::String(
                self.templater
                    .render_plain(s, ctx)
                    .unwrap_or_else(|e| panic!("var {s:?} does not render: {}", e.message())),
            ),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.render_value(v, ctx)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.render_value(v, ctx)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Modules whose `when` holds for the host, with their vars resolved.
    fn selected<'a>(&'a self, host: &HostContext) -> Vec<Selected<'a>> {
        self.revision
            .modules
            .iter()
            .filter_map(|module| {
                let scoped = host.in_module(&module.config.name, &module.config.tags);
                let vars = self.vars_for(module, &scoped);
                let ctx = scoped.with_vars(vars);
                let applies = module.config.when.as_ref().is_none_or(|w| ctx.matches(w));
                applies.then_some(Selected { module, ctx })
            })
            .collect()
    }
}

fn module_names(selected: &[Selected<'_>]) -> BTreeSet<String> {
    selected
        .iter()
        .map(|s| s.module.config.name.clone())
        .collect()
}

fn strings_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings_in(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings_in(v, out)),
        _ => {}
    }
}

/// The names in `secrets.NAME` (with or without `.present` / `.fingerprint`).
fn secret_refs(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find("secrets.") {
        let after = &rest[at + "secrets.".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            found.insert(name);
        }
        rest = after;
    }
    found
}

/// A spec as JSON, minus one field.
fn without(spec: &serde_yaml::Value, field: &str) -> Value {
    let mut json = to_json(spec);
    if let Some(map) = json.as_object_mut() {
        map.remove(field);
    }
    json
}

fn all_tasks(module: &LoadedModule) -> impl Iterator<Item = &TaskDecl> {
    module
        .config
        .tasks
        .iter()
        .chain(module.config.handlers.values())
}

// ---------------------------------------------------------------------------
// The example as a whole
// ---------------------------------------------------------------------------

#[test]
fn the_cli_validates_the_example() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_buckle"))
        .args(["validate", "--config"])
        .arg(example_dir())
        .output()
        .expect("the buckle binary runs");
    assert!(
        output.status.success(),
        "validate failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[rstest]
fn the_example_covers_the_modules_it_promises(example: Example) {
    let names: BTreeSet<String> = example
        .revision
        .modules
        .iter()
        .map(|m| m.config.name.clone())
        .collect();
    let expected = [
        "packages",
        "users",
        "ssh",
        "firewall",
        "sysctl",
        "swap",
        "terminal",
        "tailscale",
        "rustic",
        "nomad",
        "docker",
        "otel-ebpf-profiler",
    ];
    for module in expected {
        assert!(names.contains(module), "module {module} is missing");
    }
}

#[rstest]
fn every_task_and_health_kind_is_registered(example: Example) {
    for module in &example.revision.modules {
        let name = &module.config.name;
        for decl in all_tasks(module) {
            assert!(
                buckle_core::task::registry().contains_key(decl.kind.as_str()),
                "{name}: '{}' is not a registered task kind",
                decl.kind
            );
        }
        for check in &module.config.health {
            assert!(
                buckle_core::health::registry().contains_key(check.check.kind.as_str()),
                "{name}: '{}' is not a registered health check kind",
                check.check.kind
            );
        }
        for provider in &module.config.facts.provides {
            assert!(
                buckle_core::facts::registry().contains_key(provider.as_str()),
                "{name}: facts.provides names '{provider}', which no provider supplies"
            );
        }
    }
}

#[rstest]
fn module_dependencies_and_notifications_resolve(example: Example) {
    let modules: BTreeMap<&str, &LoadedModule> = example
        .revision
        .modules
        .iter()
        .map(|m| (m.config.name.as_str(), m))
        .collect();
    for (name, module) in &modules {
        for need in &module.config.needs {
            assert!(
                modules.contains_key(need.as_str()),
                "{name} needs '{need}', which is not a module"
            );
        }
        for decl in all_tasks(module) {
            for target in &decl.notify {
                let (owner, handler) = target.split_once(':').unwrap_or((name, target.as_str()));
                let owner = modules
                    .get(owner)
                    .unwrap_or_else(|| panic!("{name} notifies '{target}' in an unknown module"));
                assert!(
                    owner.config.handlers.contains_key(handler),
                    "{name} notifies '{target}', but there is no such handler"
                );
            }
        }
    }
}

#[rstest]
fn every_module_file_a_task_names_exists(example: Example) {
    for module in &example.revision.modules {
        let name = &module.config.name;
        let templates: BTreeSet<String> = module
            .templates
            .iter()
            .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
            .collect();
        for decl in all_tasks(module) {
            let src = decl.spec.get("src").and_then(|v| v.as_str());
            match (decl.kind.as_str(), src) {
                ("file.template", Some(src)) => assert!(
                    templates.contains(src),
                    "{name}: template {src} is not a .j2 file under files/"
                ),
                ("file.copy", Some(src)) => assert!(
                    example_dir()
                        .join("modules")
                        .join(name)
                        .join("files")
                        .join(src)
                        .is_file(),
                    "{name}: file {src} does not exist under files/"
                ),
                _ => {}
            }
        }
    }
}

#[rstest]
fn secrets_are_declared_and_used(example: Example) {
    let mut referenced = BTreeSet::new();
    let mut collect = |text: &str| referenced.extend(secret_refs(text));
    let mut texts = Vec::new();
    for module in &example.revision.modules {
        strings_in(&to_json(&module.config), &mut texts);
        texts.extend(module.templates.iter().map(|(_, source)| source.clone()));
    }
    strings_in(&to_json(&example.revision.fleet.fleet), &mut texts);
    strings_in(&to_json(&example.revision.fleet.vars), &mut texts);
    strings_in(&to_json(&example.revision.fleet.rollout), &mut texts);
    texts.iter().for_each(|t| collect(t));

    let declared: BTreeSet<String> = example.revision.fleet.secrets.keys().cloned().collect();
    assert_eq!(
        referenced, declared,
        "every secret that is referenced must be declared, and every declared secret used"
    );
}

#[rstest]
fn secrets_only_flow_into_secret_sinks(example: Example) {
    // A secret may appear in template files and in secret-capable spec fields
    // (user passwords, command environments), never in a plain task field,
    // a variable or a selector.
    for module in &example.revision.modules {
        for decl in all_tasks(module) {
            let mut fields = Vec::new();
            match decl.kind.as_str() {
                "user.account" => strings_in(&without(&decl.spec, "password"), &mut fields),
                "command.run" => strings_in(&without(&decl.spec, "env"), &mut fields),
                _ => strings_in(&to_json(&decl.spec), &mut fields),
            }
            let leaked: Vec<_> = fields.iter().filter(|f| f.contains("secrets.")).collect();
            assert!(
                leaked.is_empty(),
                "{}: {} puts a secret in a plain field: {leaked:?}",
                module.config.name,
                decl.kind
            );
        }
        let mut vars = Vec::new();
        strings_in(&to_json(&module.config.vars), &mut vars);
        assert!(
            vars.iter().all(|v| !v.contains("secrets.")),
            "{}: a var references a secret",
            module.config.name
        );
    }
    let mut overlays = Vec::new();
    strings_in(&to_json(&example.revision.fleet.vars), &mut overlays);
    for content in example.revision.var_files.values() {
        strings_in(&to_json(content), &mut overlays);
    }
    assert!(overlays.iter().all(|v| !v.contains("secrets.")));
}

// ---------------------------------------------------------------------------
// Per host
// ---------------------------------------------------------------------------

#[rstest]
#[case::pi_1("pi-1", &["pis", "nomad-clients", "nomad-servers"])]
#[case::pi_3("pi-3", &["pis", "nomad-clients", "nomad-servers"])]
#[case::pi_4("pi-4", &["pis", "nomad-clients", "canary"])]
#[case::oci_1("oci-1", &["cloud", "nomad-clients"])]
#[case::az_1("az-1", &["cloud", "nomad-clients"])]
fn groups_follow_labels_and_facts(example: Example, #[case] host: &str, #[case] expected: &[&str]) {
    let mut groups = example.host(host).groups;
    groups.sort();
    let mut expected: Vec<String> = expected.iter().map(|g| g.to_string()).collect();
    expected.sort();
    assert_eq!(groups, expected);
}

const EVERYWHERE: &[&str] = &[
    "packages",
    "users",
    "ssh",
    "firewall",
    "sysctl",
    "terminal",
    "tailscale",
    "nomad",
    "docker",
];

#[rstest]
#[case::pi_server("pi-1", &["rustic"])] // swap_profile is "none" on the Pis
#[case::pi_canary("pi-4", &["rustic", "swap"])] // ...except pi-4, which overrides it
#[case::oci("oci-1", &["swap", "otel-ebpf-profiler"])]
#[case::oci_batch("oci-2", &["swap", "otel-ebpf-profiler"])]
#[case::azure("az-1", &["swap", "otel-ebpf-profiler"])]
fn modules_apply_where_their_when_says(
    example: Example,
    #[case] host: &str,
    #[case] extra: &[&str],
) {
    let host = example.host(host);
    let selected = module_names(&example.selected(&host));
    let expected: BTreeSet<String> = EVERYWHERE
        .iter()
        .chain(extra)
        .map(|m| m.to_string())
        .collect();
    assert_eq!(selected, expected);
}

#[rstest]
fn a_selected_module_only_needs_selected_modules(
    example: Example,
    #[values("pi-1", "pi-4", "oci-1", "oci-2", "az-1")] host: &str,
) {
    let host = example.host(host);
    let selected = example.selected(&host);
    let names = module_names(&selected);
    for s in &selected {
        for need in &s.module.config.needs {
            assert!(
                names.contains(need),
                "{} is selected on {} but needs {need}, which is not",
                s.module.config.name,
                host.host.id
            );
        }
    }
}

/// (host, module, variable, expected value): the precedence rules in action.
#[rstest]
// module default < base: the module's own default stands until something overrides it
#[case("oci-1", "swap", "swap_profile", json!("small"))]
#[case("oci-1", "nomad", "nomad_datacenter", json!("dc1"))]
// base < platform
#[case("pi-1", "docker", "apt_distro", json!("debian"))]
#[case("oci-1", "docker", "apt_distro", json!("ubuntu"))]
#[case("az-1", "tailscale", "apt_distro", json!("ubuntu"))]
#[case("az-1", "nomad", "cni_arch", json!("amd64"))]
#[case("oci-1", "nomad", "cni_arch", json!("arm64"))]
// platform < group: oci-1 is arm64 ("arm") and a cloud host ("cloud"); the later group layer wins
#[case("oci-1", "nomad", "nomad_node_class", json!("cloud"))]
#[case("pi-4", "nomad", "nomad_node_class", json!("pi"))]
// group < group: pi-1 is in `pis` (WARN) and `nomad-servers` (DEBUG); the later, narrower layer wins
#[case("pi-1", "nomad", "nomad_log_level", json!("DEBUG"))]
#[case("pi-4", "nomad", "nomad_log_level", json!("WARN"))]
#[case("pi-1", "tailscale", "tailscale_tags", json!("tag:server"))]
#[case("pi-4", "tailscale", "tailscale_tags", json!("tag:worker"))]
// group < host
#[case("pi-1", "swap", "swap_profile", json!("none"))]
#[case("pi-4", "swap", "swap_profile", json!("small"))]
#[case("az-1", "swap", "swap_profile", json!("large"))]
#[case("oci-2", "nomad", "nomad_node_class", json!("batch"))]
// vars that reference other vars and facts
#[case("pi-1", "nomad", "nomad_server_peers", json!(["pi-1.example.ts.net", "pi-2.example.ts.net", "pi-3.example.ts.net"]))]
#[case("az-1", "otel-ebpf-profiler", "profiler_otlp_endpoint", json!("otlp.example.com:4317"))]
fn vars_resolve_by_the_documented_precedence(
    example: Example,
    #[case] host: &str,
    #[case] module: &str,
    #[case] var: &str,
    #[case] expected: Value,
) {
    let host = example.host(host);
    let module = example.module(module);
    let scoped = host.in_module(&module.config.name, &module.config.tags);
    let vars = example.vars_for(module, &scoped);
    assert_eq!(vars.get(var), Some(&expected), "{var} on {}", host.host.id);
}

#[rstest]
fn every_selected_task_renders_parses_and_has_a_unique_resource(
    example: Example,
    #[values("pi-1", "pi-4", "oci-1", "oci-2", "az-1")] host: &str,
) {
    let host = example.host(host);
    for s in example.selected(&host) {
        let name = &s.module.config.name;
        let mut seen = BTreeSet::new();
        for decl in all_tasks(s.module) {
            let task = buckle_core::task::lookup(&decl.kind).expect("kinds are registered");
            let ctx = s
                .ctx
                .in_task(&decl.kind, decl.id.as_deref().unwrap_or(""), &decl.tags);
            let mut spec = to_json(&decl.spec);
            let schema = to_json(&task.schema());
            render_spec_strings(&mut spec, &schema, &mut |text| {
                example.templater.render_plain(text, &ctx)
            })
            .unwrap_or_else(|e| {
                panic!(
                    "{name}: a {} field does not render: {}",
                    decl.kind,
                    e.message()
                )
            });
            let parsed = task.parse(spec).unwrap_or_else(|e| {
                panic!("{name}: a {} spec is invalid: {}", decl.kind, e.message())
            });
            let active = decl.when.as_ref().is_none_or(|w| ctx.matches(w));
            if active
                && s.module
                    .config
                    .handlers
                    .values()
                    .all(|h| !std::ptr::eq(h, decl))
            {
                let key = resource_key(name, task, &parsed, decl.id.as_deref());
                assert!(
                    seen.insert(key.to_string()),
                    "{name}: two tasks manage {key}; give one an explicit id or merge them"
                );
            }
        }
        for check in &s.module.config.health {
            let kind =
                buckle_core::health::lookup(&check.check.kind).expect("kinds are registered");
            let mut spec = to_json(&check.check.spec);
            render_all(&example, &mut spec, &s.ctx);
            kind.parse(spec).unwrap_or_else(|e| {
                panic!(
                    "{name}: health check {} is invalid: {}",
                    check.display_name(),
                    e.message()
                )
            });
        }
    }
}

fn render_all(example: &Example, value: &mut Value, ctx: &HostContext) {
    *value = example.render_value(value, ctx);
}

#[rstest]
fn every_template_renders_on_every_host_that_selects_its_module(
    example: Example,
    #[values("pi-1", "pi-4", "oci-1", "oci-2", "az-1")] host: &str,
) {
    let host = example.host(host);
    for s in example.selected(&host) {
        for (path, source) in &s.module.templates {
            let rendered = example
                .templater
                .render_sensitive(source, &s.ctx)
                .unwrap_or_else(|e| {
                    panic!(
                        "{}: {} does not render on {}: {}",
                        s.module.config.name,
                        path.display(),
                        host.host.id,
                        e.message()
                    )
                });
            assert!(
                rendered.is_sensitive() == !secret_refs(source).is_empty(),
                "{}: {} should be sensitive exactly when it references a secret",
                s.module.config.name,
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Rollout
// ---------------------------------------------------------------------------

#[rstest]
#[case("pi-4", "canary")]
#[case("pi-1", "pis")]
#[case("pi-2", "pis")]
#[case("pi-3", "pis")]
#[case("oci-1", "cloud")]
#[case("oci-2", "cloud")]
#[case("az-1", "cloud")]
fn a_host_belongs_to_the_first_phase_that_selects_it(
    example: Example,
    #[case] host: &str,
    #[case] phase: &str,
) {
    let host = example.host(host);
    let first = example
        .revision
        .fleet
        .rollout
        .phases
        .iter()
        .find(|p| host.matches(&p.select))
        .map(|p| p.name.as_str());
    assert_eq!(first, Some(phase), "phase of {}", host.host.id);
}

#[rstest]
fn the_canary_phase_needs_a_person_and_the_cloud_phase_guards_removals(example: Example) {
    let rollout = &example.revision.fleet.rollout;
    let approval = |name: &str| {
        rollout
            .phases
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no phase called {name}"))
            .approval
    };
    assert_eq!(approval("canary"), ApprovalPolicy::Required);
    assert_eq!(approval("cloud"), ApprovalPolicy::IfDestructive);
    assert!(rollout.local_rollback.enabled);
    assert!(rollout.health.bake > std::time::Duration::ZERO);
}
