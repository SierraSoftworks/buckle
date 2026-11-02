//! `buckle migrate`: behaviour on small v1 fixtures (`fixtures/v1/`).
//!
//! Each test runs the real binary on a fixture and looks at what an operator
//! would: whether the output loads and validates as a v2 repository, which
//! tasks the v1 constructs became, what the converted templates render to,
//! what is flagged with `TODO(migrate)`, and how it refuses to overwrite.

use buckle_core::engine::loader::load_dir;
use buckle_core::engine::{LoadedModule, Revision};
use buckle_core::plan::RevisionId;
use buckle_core::secrets::{InstallKey, Secret, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity, Vars};
use buckle_core::template::Templater;
use rstest::rstest;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/v1")
        .join(name)
}

struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn migrate(v1: &Path, out: &Path, extra: &[&str]) -> Ran {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_buckle"))
        .arg("migrate")
        .arg(v1)
        .arg(out)
        .args(extra)
        .output()
        .expect("the buckle binary runs");
    Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Migrates a fixture into a fresh directory and returns it.
fn migrated(name: &str) -> tempfile::TempDir {
    let out = tempfile::tempdir().expect("a temp dir");
    let ran = migrate(&fixture(name), out.path(), &[]);
    assert!(ran.ok, "migrating {name} failed:\n{}", ran.stderr);
    out
}

fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn text(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|_| panic!("{rel} was not written"))
}

fn load(dir: &Path) -> Revision {
    buckle_std::ensure_linked();
    load_dir(dir, RevisionId::new("migrated")).expect("the output loads as a v2 repository")
}

fn module<'a>(revision: &'a Revision, name: &str) -> &'a LoadedModule {
    revision
        .modules
        .iter()
        .find(|m| m.config.name == name)
        .unwrap_or_else(|| panic!("no module {name}"))
}

/// `(kind, dest)` of each task, in order.
fn tasks(module: &LoadedModule) -> Vec<(String, String)> {
    module
        .config
        .tasks
        .iter()
        .map(|t| {
            let dest = t
                .spec
                .get("dest")
                .or_else(|| t.spec.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            (t.kind.clone(), dest.to_owned())
        })
        .collect()
}

fn render(
    module: &LoadedModule,
    template: &str,
    vars: serde_json::Value,
    secrets: &[(&str, &str)],
) -> String {
    let source = module
        .templates
        .iter()
        .find(|(path, _)| path.to_string_lossy().replace('\\', "/") == template)
        .unwrap_or_else(|| panic!("no template {template}"))
        .1
        .clone();
    let key = InstallKey::from_bytes([1u8; 32]);
    let mut store = SecretStore::new();
    for (name, value) in secrets {
        store.insert(Secret::new(*name, value.to_string(), &key));
    }
    let vars: Vars = serde_json::from_value(vars).expect("vars are a flat map");
    let ctx = HostContext::new(HostIdentity::default())
        .with_vars(vars)
        .with_secrets(Arc::new(store));
    // Redacted rendering: secrets show as `«secret:NAME@fingerprint»` tokens, so
    // the text can be compared without ever holding a secret value.
    Templater::new()
        .render_redacted(&source, &ctx)
        .unwrap_or_else(|e| panic!("{template} does not render: {}", e.message()))
        .into_inner()
}

// ---------------------------------------------------------------------------
// What a clean migration produces
// ---------------------------------------------------------------------------

#[rstest]
fn a_migration_loads_and_validates_as_a_v2_repository(
    #[values("minimal", "files", "config", "secrets", "scripts", "needs", "ignored")] name: &str,
) {
    let out = migrated(name);
    let revision = load(out.path());
    assert_eq!(revision.fleet.version, 2);
    let report = buckle_core::engine::validate(&revision).expect("validation runs");
    assert!(report.is_ok(), "{name}: {report}");
}

#[test]
fn packages_become_modules_with_their_description_and_dependencies() {
    let out = migrated("needs");
    let revision = load(out.path());
    let names: Vec<&str> = revision
        .modules
        .iter()
        .map(|m| m.config.name.as_str())
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
    assert_eq!(module(&revision, "b").config.needs, ["a"]);
    assert_eq!(module(&revision, "c").config.needs, ["a", "b"]);
    assert_eq!(module(&revision, "c").config.description, "top");

    let out = migrated("minimal");
    let solo = &load(out.path()).modules[0].config;
    assert!(solo.description.contains("The only package."));
    assert!(solo.description.contains("It does nothing yet."));
}

#[test]
fn files_land_in_their_group_target_and_templates_become_file_templates() {
    let out = migrated("files");
    let revision = load(out.path());
    let web = module(&revision, "web");
    let tasks = tasks(web);
    for expected in [
        ("file.copy", "/etc/web.d/app.conf"),
        ("file.copy", "/etc/web.d/nested/extra.conf"),
        ("file.template", "/etc/web.d/site.conf"),
        ("file.copy", "/usr/local/bin/run"),
        // a group package.yml does not map was written relative to "/"
        ("file.copy", "/motd"),
    ] {
        assert!(
            tasks.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing {expected:?} in {tasks:?}"
        );
    }
    assert!(
        !tasks.iter().any(|(_, dest)| dest.contains("stray")),
        "files outside a group were never installed by v1"
    );
    // every source a task names was carried over
    for task in &web.config.tasks {
        let src = task.spec.get("src").and_then(|v| v.as_str()).unwrap();
        assert!(
            out.path().join("modules/web/files").join(src).is_file(),
            "{src} was not copied"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_executable_file_keeps_its_mode() {
    let out = migrated("files");
    let revision = load(out.path());
    let run = module(&revision, "web")
        .config
        .tasks
        .iter()
        .find(|t| t.spec.get("dest").and_then(|v| v.as_str()) == Some("/usr/local/bin/run"))
        .unwrap();
    assert_eq!(run.spec.get("mode").and_then(|v| v.as_str()), Some("0755"));
}

#[test]
fn go_template_references_render_to_what_v1_rendered() {
    let out = migrated("files");
    let revision = load(out.path());
    let rendered = render(
        module(&revision, "web"),
        "conf.d/site.conf.j2",
        json!({"SITE": "example.com"}),
        &[],
    );
    assert_eq!(rendered, "server_name = example.com\n");
}

#[test]
fn global_config_becomes_an_overlay_and_package_config_module_vars() {
    let out = migrated("config");
    let revision = load(out.path());
    assert_eq!(
        revision.fleet.vars.len(),
        2,
        "one file overlay, one precedence overlay"
    );
    assert!(
        revision
            .var_files
            .contains_key(Path::new("vars/defaults.yaml"))
    );

    // v1 let a package's config override the global config of the same name.
    // Resolve the way the engine does (module vars, then overlays in order):
    let app = module(&revision, "app");
    let mut vars: Vars = app
        .config
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap()))
        .collect();
    for overlay in &revision.fleet.vars {
        let ctx = HostContext::new(HostIdentity::default())
            .in_module("app", &[])
            .with_vars(vars.clone());
        if overlay.when.as_ref().is_some_and(|w| !ctx.matches(w)) {
            continue;
        }
        if let Some(file) = &overlay.file {
            let content = serde_json::to_value(&revision.var_files[file]).unwrap();
            for (k, v) in content.as_object().unwrap() {
                vars.insert(k.clone(), v.clone());
            }
        }
        for (k, v) in &overlay.values {
            vars.insert(k.clone(), serde_json::to_value(v).unwrap());
        }
    }
    assert_eq!(vars["SHARED"], json!("from-package"));
    assert_eq!(vars["A"], json!("1"));
    assert_eq!(vars["B"], json!("2"));

    let rendered = render(
        app,
        "etc/app.conf.j2",
        json!({"A": "1", "B": "2", "SHARED": "from-package"}),
        &[],
    );
    assert_eq!(rendered, "a=1 b=2 shared=from-package\n");
}

#[test]
fn awkward_variable_names_are_renamed_and_flagged() {
    let out = migrated("config");
    let vars = text(out.path(), "vars/defaults.yaml");
    assert!(vars.contains("dotted_key"));
    assert!(vars.contains("TODO(migrate)"), "{vars}");
}

#[test]
fn secrets_are_declared_by_name_and_their_values_are_never_copied() {
    let out = migrated("secrets");
    for (path, contents) in tree(out.path()) {
        let contents = String::from_utf8_lossy(&contents);
        for value in ["hunter2", "swordfish"] {
            assert!(
                !contents.contains(value),
                "{} leaks a secret value",
                path.display()
            );
        }
    }
    let revision = load(out.path());
    let declared: Vec<&str> = revision.fleet.secrets.keys().map(String::as_str).collect();
    assert_eq!(declared, ["API_KEY", "TOKEN"]);
    assert!(text(out.path(), "buckle.yaml").contains("TODO(migrate)"));

    // v1 merged secrets over config in templates; they are secrets.X now.
    let rendered = render(
        module(&revision, "api"),
        "etc/api.conf.j2",
        json!({"REGION": "eu"}),
        &[("TOKEN", "t0ken"), ("API_KEY", "k3y")],
    );
    assert!(rendered.contains("token=\u{ab}secret:TOKEN@"), "{rendered}");
    assert!(
        rendered.contains("\nkey=\u{ab}secret:API_KEY@"),
        "{rendered}"
    );
    assert!(rendered.ends_with("\nregion=eu\n"), "{rendered}");
}

#[test]
fn scripts_run_after_the_files_in_name_order_with_the_environment_v1_gave_them() {
    let out = migrated("scripts");
    let revision = load(out.path());
    let svc = module(&revision, "svc");
    let commands: Vec<_> = svc
        .config
        .tasks
        .iter()
        .filter(|t| t.kind == "command.run")
        .collect();
    let runs: Vec<String> = commands
        .iter()
        .map(|t| {
            let argv = t.spec.get("run").and_then(|r| r.as_sequence()).unwrap();
            argv.iter()
                .map(|a| a.as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        runs,
        [
            "bash /var/lib/buckle/scripts/svc/10-install.sh",
            "bash /var/lib/buckle/scripts/svc/20-start.sh",
            "pwsh /var/lib/buckle/scripts/svc/30-windows.ps1",
        ]
    );
    for command in &commands {
        let env = command.spec.get("env").expect("config reaches the script");
        assert_eq!(
            env.get("VERSION").and_then(|v| v.as_str()),
            Some("{{ vars.VERSION }}")
        );
        assert_eq!(
            env.get("ENV").and_then(|v| v.as_str()),
            Some("{{ vars.ENV }}")
        );
        // v1 retried the package; v2 retries the task that can fail.
        assert_eq!(command.retry.limit, 3);
        assert_eq!(command.retry.delay, std::time::Duration::from_millis(250));
    }
    // every script is copied to the host before it runs
    let kinds: Vec<_> = svc.config.tasks.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds[..2], ["file.copy", "command.run"]);
    assert!(text(out.path(), "modules/svc/module.yaml").contains("TODO(migrate)"));
}

#[test]
fn secrets_reach_scripts_through_the_secret_environment() {
    let out = migrated("secrets");
    let revision = load(out.path());
    let command = module(&revision, "api")
        .config
        .tasks
        .iter()
        .find(|t| t.kind == "command.run")
        .unwrap();
    let env = command.spec.get("env").unwrap();
    assert_eq!(
        env.get("TOKEN").and_then(|v| v.as_str()),
        Some("{{ secrets.TOKEN }}")
    );
    assert_eq!(
        env.get("REGION").and_then(|v| v.as_str()),
        Some("{{ vars.REGION }}")
    );
}

// ---------------------------------------------------------------------------
// Nothing that needs a person is dropped silently
// ---------------------------------------------------------------------------

#[test]
fn go_template_actions_without_a_jinja_twin_are_kept_and_flagged() {
    let out = migrated("go-template");
    let template = text(out.path(), "modules/tpl/files/etc/tpl.conf.j2");
    assert!(
        template.contains("{{ if .ENABLED }}"),
        "the action is kept as written"
    );
    assert!(template.contains("TODO(migrate)"));
    assert!(text(out.path(), "modules/tpl/module.yaml").contains("TODO(migrate)"));
    // plain text that Jinja would misread is escaped, not left to break the template
    assert!(template.contains("{{ '{#' }}"));
    assert!(template.contains("{{ '{%' }}"));
}

#[test]
fn config_scripts_are_copied_aside_and_flagged() {
    let out = migrated("config-script");
    assert_eq!(
        text(out.path(), "facts.d/global/azure.sh"),
        std::fs::read_to_string(fixture("config-script").join("config/azure.sh")).unwrap()
    );
    assert!(text(out.path(), "buckle.yaml").contains("TODO(migrate)"));
}

#[test]
fn entries_v1_never_read_are_reported_not_lost_in_silence() {
    let out = tempfile::tempdir().unwrap();
    let ran = migrate(&fixture("ignored"), out.path(), &[]);
    assert!(ran.ok, "{}", ran.stderr);
    for ignored in ["README.md", "stray.txt", "subdir", "docs", "loose.txt"] {
        let shown = ran.stdout.contains(ignored);
        let written = text(out.path(), "buckle.yaml").contains(ignored);
        assert!(
            shown && written,
            "{ignored} should be reported on stdout and in buckle.yaml"
        );
    }
    // a package.yml key v1 ignored is preserved as a comment
    assert!(text(out.path(), "modules/p/module.yaml").contains("owner"));
}

#[test]
fn the_summary_counts_what_needs_a_decision() {
    let out = tempfile::tempdir().unwrap();
    let ran = migrate(&fixture("go-template"), out.path(), &[]);
    assert!(ran.ok);
    assert!(ran.stdout.contains("TODO(migrate)"), "{}", ran.stdout);
    let clean = tempfile::tempdir().unwrap();
    let ran = migrate(&fixture("minimal"), clean.path(), &[]);
    assert!(!ran.stdout.contains("TODO(migrate)"), "{}", ran.stdout);
}

// ---------------------------------------------------------------------------
// Failure modes
// ---------------------------------------------------------------------------

#[rstest]
#[case::missing_dependency("bad-needs", "missing")]
#[case::dependency_cycle("cycle", "cycle")]
#[case::script_v1_could_not_run("bad-script-extension", "setup.py")]
#[case::config_v1_could_not_load("bad-config-extension", "notes.txt")]
#[case::unreadable_package_yml("bad-package-yaml", "package.yml")]
#[case::package_without_package_yml("no-package-yml", "package.yml")]
#[case::not_a_v1_directory("not-v1", "packages")]
fn input_v1_could_not_run_is_refused_and_nothing_is_written(
    #[case] name: &str,
    #[case] mentions: &str,
) {
    let parent = tempfile::tempdir().unwrap();
    let out = parent.path().join("out");
    let ran = migrate(&fixture(name), &out, &[]);
    assert!(!ran.ok, "{name} should be refused");
    assert!(ran.stderr.contains(mentions), "{}", ran.stderr);
    assert!(
        !out.exists(),
        "a refused migration must not create the output directory"
    );
}

#[test]
fn a_missing_v1_directory_is_refused() {
    let parent = tempfile::tempdir().unwrap();
    let ran = migrate(&parent.path().join("nope"), &parent.path().join("out"), &[]);
    assert!(!ran.ok);
}

// ---------------------------------------------------------------------------
// The output directory
// ---------------------------------------------------------------------------

#[test]
fn a_non_empty_output_directory_is_not_overwritten_without_force() {
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("buckle.yaml"), "mine: true\n").unwrap();
    let before = tree(out.path());
    let ran = migrate(&fixture("files"), out.path(), &[]);
    assert!(!ran.ok);
    assert!(ran.stderr.contains("--force"), "{}", ran.stderr);
    assert_eq!(tree(out.path()), before, "nothing may change");
}

#[test]
fn an_empty_existing_output_directory_is_fine() {
    let out = tempfile::tempdir().unwrap();
    assert!(migrate(&fixture("files"), out.path(), &[]).ok);
    assert!(out.path().join("buckle.yaml").is_file());
}

#[test]
fn force_replaces_what_migrate_owns_and_leaves_the_rest() {
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("buckle.yaml"), "version: 1\n").unwrap();
    std::fs::create_dir_all(out.path().join("modules/stale")).unwrap();
    std::fs::write(
        out.path().join("modules/stale/module.yaml"),
        "name: stale\n",
    )
    .unwrap();
    std::fs::write(out.path().join("notes.txt"), "keep me\n").unwrap();
    let ran = migrate(&fixture("files"), out.path(), &["--force"]);
    assert!(ran.ok, "{}", ran.stderr);
    assert!(text(out.path(), "buckle.yaml").contains("version: 2"));
    assert!(!out.path().join("modules/stale").exists());
    assert!(out.path().join("modules/web/module.yaml").is_file());
    assert_eq!(text(out.path(), "notes.txt"), "keep me\n");
}

#[test]
fn running_the_same_migration_again_changes_nothing() {
    let out = tempfile::tempdir().unwrap();
    assert!(migrate(&fixture("scripts"), out.path(), &[]).ok);
    let first = tree(out.path());

    // without --force: the directory already holds exactly this, so it is a no-op
    let again = migrate(&fixture("scripts"), out.path(), &[]);
    assert!(again.ok, "{}", again.stderr);
    assert_eq!(tree(out.path()), first);

    // with --force: rewritten, byte for byte the same
    assert!(migrate(&fixture("scripts"), out.path(), &["--force"]).ok);
    assert_eq!(tree(out.path()), first);
}

#[test]
fn the_output_cannot_be_the_v1_directory() {
    let v1 = fixture("minimal");
    let before = tree(&v1);
    let ran = migrate(&v1, &v1, &["--force"]);
    assert!(!ran.ok);
    assert_eq!(tree(&v1), before, "the v1 tree must stay intact");
}

#[test]
fn an_output_path_that_is_a_file_is_refused() {
    let parent = tempfile::tempdir().unwrap();
    let file = parent.path().join("out");
    std::fs::write(&file, "x").unwrap();
    assert!(!migrate(&fixture("minimal"), &file, &[]).ok);
}
