//! `buckle plan|apply|health|facts|validate`: the engine against a local
//! configuration directory, with the real host behind it.
//!
//! Exit codes: `plan` returns 0 when nothing would change and 2 when it would
//! (so CI can detect drift); the others return 0 on success and 1 on failure.
//! Errors propagate as human errors and exit 1.

use crate::render;
use buckle_core::engine::{ApplyEvent, Engine, Inputs, Revision};
use buckle_core::facts::Facts;
use buckle_core::health::HealthScope;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::RevisionId;
use buckle_core::secrets::providers::{EnvProvider, FileProvider};
use buckle_core::secrets::{InstallKey, Resolver, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity, Labels};
use buckle_core::state::{FileStateStore, StateStore};
use buckle_core::system::{RealSystem, System};
use buckle_core::task::{Cx, EmptySource, Mode};
use clap::Args;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

/// Arguments shared by the local commands.
#[derive(Args, Debug, Clone)]
pub struct LocalArgs {
    /// The configuration directory (containing buckle.yaml).
    #[arg(short, long, env = "BUCKLE_CONFIG", value_name = "DIR")]
    pub config: PathBuf,

    /// Host identity to plan as; defaults to the hostname.
    #[arg(long)]
    pub host_id: Option<String>,

    /// Labels for this host, as comma-separated key=value pairs. Values are
    /// typed like YAML scalars: `canary=true` is a boolean, `rack=3` a number,
    /// `rack="3"` a string.
    #[arg(long, value_parser = parse_labels, default_value = "")]
    pub labels: BTreeMap<String, String>,

    /// Restrict to tasks matching this filt-rs selector, e.g. 'task.tags contains "network"'.
    #[arg(long)]
    pub select: Option<String>,

    /// Where the agent keeps its state (resources, shadows, facts cache).
    #[arg(long, env = "BUCKLE_STATE_DIR", default_value = default_state_dir())]
    pub state_dir: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Use these facts (a JSON file) instead of collecting them; for authoring and tests.
    #[arg(long, value_name = "FILE")]
    pub facts: Option<PathBuf>,
}

fn default_state_dir() -> &'static str {
    if cfg!(windows) {
        r"C:\ProgramData\buckle"
    } else {
        "/var/lib/buckle"
    }
}

pub fn parse_labels(s: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("label '{pair}' must be key=value"))?;
        out.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    Ok(out)
}

#[derive(Args, Debug, Clone)]
pub struct PlanArgs {
    #[command(flatten)]
    pub local: LocalArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub local: LocalArgs,
    /// Apply without confirming removals of packages, users or groups.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Waive these checks (module/check) for this run.
    #[arg(long = "waive")]
    pub waive_checks: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// The configuration directory. Accepted so that `status` takes the same
    /// flags as `plan` and `apply`; the report itself comes from the state
    /// directory.
    #[arg(short, long, env = "BUCKLE_CONFIG", value_name = "DIR")]
    pub config: Option<PathBuf>,

    /// Where the agent keeps its state (resources, shadows, facts cache).
    #[arg(long, env = "BUCKLE_STATE_DIR", default_value = default_state_dir())]
    pub state_dir: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
}

#[derive(Args, Debug, Clone)]
pub struct HealthArgs {
    #[command(flatten)]
    pub local: LocalArgs,
    /// Only these modules.
    #[arg(long = "module")]
    pub modules: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct FactsArgs {
    /// Only these providers.
    #[arg(long = "provider")]
    pub providers: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ValidateArgs {
    /// The configuration directory (containing buckle.yaml).
    #[arg(short, long, env = "BUCKLE_CONFIG", value_name = "DIR")]
    pub config: PathBuf,
    /// Facts files to validate selectors and templates against.
    #[arg(long = "facts", value_name = "FILE")]
    pub facts: Vec<PathBuf>,
    /// Labels applied to every --facts host, as comma-separated key=value
    /// pairs (typed like `plan --labels`).
    #[arg(long, value_parser = parse_labels, default_value = "")]
    pub labels: BTreeMap<String, String>,
}

impl std::ops::Deref for PlanArgs {
    type Target = LocalArgs;
    fn deref(&self) -> &LocalArgs {
        &self.local
    }
}

/// The revision label for a local directory.
const WORKING_TREE: &str = "working-tree";

/// A label value: `true`, `42` and `1.5` become typed JSON, a quoted value
/// stays the string inside the quotes, everything else is a string (the same
/// rule as [`buckle_core::selectors::label_value`]).
fn label_value(raw: &str) -> serde_json::Value {
    buckle_core::selectors::label_value(raw)
}

fn load_facts_file(path: &Path) -> buckle_core::Result<Facts> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("The facts file {} could not be read.", path.display()),
            &["Pass a JSON file with the facts tree, e.g. one from fixtures/facts/."],
        )
    })?;
    let tree: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("The facts file {} is not valid JSON.", path.display()),
            &["The file must contain one JSON object: the facts tree mounted at host.facts."],
        )
    })?;
    if !tree.is_object() {
        return Err(buckle_core::error::user(
            format!(
                "The facts file {} must contain a JSON object.",
                path.display()
            ),
            &["Each top-level key is a fact provider, e.g. {\"os\": {\"family\": \"debian\"}}."],
        ));
    }
    Ok(Facts {
        tree,
        collected_at: Default::default(),
        stale: Default::default(),
    })
}

fn load_revision(config: &Path) -> buckle_core::Result<Arc<Revision>> {
    let revision = buckle_core::engine::loader::load_dir(config, RevisionId::new(WORKING_TREE))?;
    buckle_core::engine::validate(&revision)?.into_result()?;
    Ok(Arc::new(revision))
}

fn real_system() -> buckle_core::Result<Arc<dyn System>> {
    Ok(Arc::new(
        RealSystem::new().with_http(Arc::new(crate::http::ReqwestClient::new())),
    ))
}

/// The install key fingerprints secrets. When the state directory is not
/// writable (planning as an unprivileged user), fall back to a throwaway key
/// so plans still work; fingerprints then differ between runs.
fn install_key(state_dir: &Path) -> InstallKey {
    match InstallKey::load_or_create(&state_dir.join("install.key")) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err.description(),
                "using a temporary install key; secret fingerprints will not be stable"
            );
            InstallKey::generate()
        }
    }
}

/// Everything the local commands share: the engine inputs for this host,
/// built from a validated revision.
struct Local {
    inputs: Inputs,
}

impl Local {
    async fn load(args: &LocalArgs) -> buckle_core::Result<Self> {
        let revision = load_revision(&args.config)?;
        let system = real_system()?;
        let host = HostIdentity {
            id: args.host_id.clone().unwrap_or_else(|| system.hostname()),
            labels: args
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), label_value(v)))
                .collect::<Labels>(),
        };
        let store = Arc::new(FileStateStore::new(args.state_dir.join("state.cbor")));
        let cached_facts = store.load()?.facts;
        let secrets = Resolver::new(install_key(&args.state_dir))
            .with_provider(EnvProvider)
            .with_provider(FileProvider::default())
            .resolve_all(&revision.fleet.secrets)
            .await?;
        let select = match &args.select {
            Some(text) => Some(buckle_core::Filter::new(text.clone()).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("The selector '{text}' is not a valid filt-rs expression."),
                    &["Example: --select 'task.tags contains \"network\"'."],
                )
            })?),
            None => None,
        };
        let mut inputs = Inputs::new(revision.clone(), host, system, Arc::new(secrets), store)
            .with_select(select)
            .with_facts(cached_facts);
        if let Some(path) = &args.facts {
            inputs = inputs.with_frozen_facts(load_facts_file(path)?);
        }
        Ok(Self { inputs })
    }
}

pub async fn plan(args: PlanArgs) -> buckle_core::Result<i32> {
    plan_to(args, &mut std::io::stdout()).await
}

pub(crate) async fn plan_to(args: PlanArgs, out: &mut dyn Write) -> buckle_core::Result<i32> {
    let local = Local::load(&args.local).await?;
    let (plan, facts) = Engine::plan(&local.inputs).await?;
    match args.output {
        OutputFormat::Text => {
            let _ = out
                .write_all(render::render_plan(&plan, WORKING_TREE, Some(&facts.tree)).as_bytes());
        }
        OutputFormat::Json => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&plan).unwrap_or_default()
            );
        }
    }
    Ok(if plan.is_empty() { 0 } else { 2 })
}

pub async fn apply(args: ApplyArgs) -> buckle_core::Result<i32> {
    let mut local = Local::load(&args.local).await?;
    local.inputs = local.inputs.with_waived_checks(args.waive_checks.clone());
    let (plan, facts) = Engine::plan(&local.inputs).await?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(render::render_plan(&plan, WORKING_TREE, Some(&facts.tree)).as_bytes());
    drop(out);
    if plan.is_empty() {
        println!("nothing to do");
        return Ok(0);
    }
    let cautionary: Vec<String> = plan
        .removals
        .iter()
        .filter(|r| r.caution && r.policy == RemovalPolicy::Cleanup)
        .map(|r| r.resource.to_string())
        .collect();
    if !cautionary.is_empty() && !args.yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Err(buckle_core::error::user(
                format!(
                    "This plan removes packages, users or groups ({}) and no terminal is attached to confirm it.",
                    cautionary.join(", ")
                ),
                &[
                    "Re-run with --yes to confirm the removals, or change the configuration so that they are not needed.",
                ],
            ));
        }
        print!(
            "This plan removes packages, users or groups ({}). Proceed? [y/N] ",
            cautionary.join(", ")
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(1);
        }
    }

    println!();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApplyEvent>(256);
    let interactive = {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal()
    };
    let printer = tokio::spawn(async move {
        let mut live = render::LiveRenderer::new(interactive);
        while let Some(event) = rx.recv().await {
            print!("{}", live.event(&event));
            let _ = std::io::stdout().flush();
        }
        let mut rest = String::new();
        live.close(&mut rest);
        print!("{rest}");
    });
    let outcome = Engine::apply(&local.inputs, &plan, tx).await;
    let _ = printer.await;
    let outcome = outcome?;
    println!();
    print!("{}", render::render_outcome(&outcome));
    if let Some(report) = &outcome.health
        && !report.results.is_empty()
    {
        println!();
        print!(
            "{}",
            render::render_health(report, &local.inputs.waived_checks)
        );
    }
    Ok(if outcome.succeeded { 0 } else { 1 })
}

pub async fn status(args: StatusArgs) -> buckle_core::Result<i32> {
    status_to(args, &mut std::io::stdout())
}

/// Prints the report of the last apply on this host (by `buckle apply` or
/// the agent) from the state directory. Exits 1 when that run did not
/// succeed.
pub(crate) fn status_to(args: StatusArgs, out: &mut dyn Write) -> buckle_core::Result<i32> {
    let store = FileStateStore::new(args.state_dir.join("state.cbor"));
    let state = store.load()?;
    let last = store.load_last_run()?;
    let run = state.runs.last();
    if args.output == OutputFormat::Json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "run": run, "outcome": last }))
                .unwrap_or_default()
        );
    } else {
        match run {
            Some(run) => {
                let _ = writeln!(
                    out,
                    "last run  revision {} · finished {} · {}",
                    run.revision,
                    run.finished_at.format("%Y-%m-%d %H:%M:%S UTC"),
                    serde_json::to_value(run.outcome)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
                        .unwrap_or_default()
                );
            }
            None if last.is_none() => {
                let _ = writeln!(out, "no run is recorded in {}", args.state_dir.display());
                return Ok(0);
            }
            None => {}
        }
        match &last {
            Some(outcome) => {
                let _ = writeln!(out);
                let _ = out.write_all(render::render_outcome(outcome).as_bytes());
            }
            None => {
                if let Some(error) = run.and_then(|r| r.error.as_ref()) {
                    let _ = writeln!(out, "error: {error}");
                }
                let _ = writeln!(
                    out,
                    "(no step detail was kept for this run; the next apply keeps it)"
                );
            }
        }
    }
    let succeeded = match (&last, run) {
        (Some(outcome), _) => outcome.succeeded,
        (None, Some(run)) => run.outcome == buckle_core::state::RunOutcome::Succeeded,
        (None, None) => true,
    };
    Ok(if succeeded { 0 } else { 1 })
}

pub async fn health(args: HealthArgs) -> buckle_core::Result<i32> {
    health_to(args, &mut std::io::stdout()).await
}

pub(crate) async fn health_to(args: HealthArgs, out: &mut dyn Write) -> buckle_core::Result<i32> {
    let local = Local::load(&args.local).await?;
    let modules = (!args.modules.is_empty()).then_some(args.modules.as_slice());
    let report = Engine::health(&local.inputs, HealthScope::Manual, modules).await?;
    match args.local.output {
        OutputFormat::Text => {
            let _ = out.write_all(render::render_health(&report, &[]).as_bytes());
        }
        OutputFormat::Json => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
        }
    }
    Ok(if report.healthy(&[]) { 0 } else { 1 })
}

pub async fn facts(args: FactsArgs) -> buckle_core::Result<i32> {
    facts_to(args, &mut std::io::stdout()).await
}

pub(crate) async fn facts_to(args: FactsArgs, out: &mut dyn Write) -> buckle_core::Result<i32> {
    buckle_std::ensure_linked();
    let system = real_system()?;
    let cx = Cx {
        mode: Mode::Plan,
        host: Arc::new(HostContext::new(HostIdentity {
            id: system.hostname(),
            labels: Labels::new(),
        })),
        system,
        templater: Arc::new(buckle_core::template::Templater::new()),
        source: Arc::new(EmptySource),
        module: "facts".into(),
        secrets: Arc::new(SecretStore::new()),
        waived_checks: Arc::new(Vec::new()),
        previous: None,
    };
    for provider in &args.providers {
        if !buckle_core::facts::registry().contains_key(provider.as_str()) {
            return Err(buckle_core::error::user(
                format!("'{provider}' is not a fact provider this Buckle knows about."),
                &["Run `buckle catalog` to list the available providers."],
            ));
        }
    }
    let only = (!args.providers.is_empty()).then_some(args.providers.as_slice());
    let mut facts = Facts::new();
    let errors = buckle_core::facts::collect(&cx, &mut facts, only, true).await;
    for error in &errors {
        eprintln!(
            "warning: fact provider '{}' failed: {}",
            error.provider, error.message
        );
    }
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&facts.tree).unwrap_or_default()
    );
    Ok(0)
}

pub async fn validate(args: ValidateArgs) -> buckle_core::Result<i32> {
    validate_to(args, &mut std::io::stdout()).await
}

pub(crate) async fn validate_to(
    args: ValidateArgs,
    out: &mut dyn Write,
) -> buckle_core::Result<i32> {
    buckle_std::ensure_linked();
    let revision =
        buckle_core::engine::loader::load_dir(&args.config, RevisionId::new(WORKING_TREE))?;
    let mut report = buckle_core::engine::validate(&revision)?;
    let labels: Labels = args
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), label_value(v)))
        .collect();
    let mut summaries = Vec::new();
    if report.is_ok() {
        for path in &args.facts {
            let facts = load_facts_file(path)?;
            let host = HostIdentity {
                id: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "validate".into()),
                labels: labels.clone(),
            };
            let (mut with_facts, summary) =
                buckle_core::engine::validate_host(&revision, &host, &facts).await?;
            if let Some(summary) = summary {
                summaries.push((path.clone(), summary));
            }
            for issue in with_facts
                .errors
                .iter_mut()
                .chain(with_facts.warnings.iter_mut())
            {
                issue.location = format!("{} ({})", issue.location, path.display());
            }
            report.merge(with_facts);
        }
    }
    let _ = write!(out, "{report}");
    for (path, s) in &summaries {
        let list = |v: &[String]| {
            if v.is_empty() {
                "none".to_owned()
            } else {
                v.join(", ")
            }
        };
        let _ = writeln!(
            out,
            "host {} ({}): groups {}; selected {}",
            s.host,
            path.display(),
            list(&s.groups),
            list(&s.selected)
        );
        if !s.conditional.is_empty() {
            let _ = writeln!(
                out,
                "  decided on the host after a provider applies: {}",
                s.conditional.join(", ")
            );
        }
        for i in &s.indeterminate {
            let _ = writeln!(out, "  indeterminate: {} ({})", i.module, i.reason);
        }
    }
    let _ = writeln!(
        out,
        "{} modules in {}: {} error{}, {} warning{}",
        revision.modules.len(),
        args.config.display(),
        report.errors.len(),
        if report.errors.len() == 1 { "" } else { "s" },
        report.warnings.len(),
        if report.warnings.len() == 1 { "" } else { "s" },
    );
    Ok(if report.is_ok() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use buckle_core::plan::{Diff, Plan, Planned};
    use buckle_core::task::{Applied, Task};
    use rstest::rstest;
    use serde::{Deserialize, Serialize};

    /// A read-only test kind: plans a create when the path is missing.
    #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    struct Spec {
        path: String,
    }

    struct CliTestFile;

    #[async_trait]
    impl Task for CliTestFile {
        type Spec = Spec;
        const KIND: &'static str = "cli.test_file";
        fn key(spec: &Spec) -> String {
            spec.path.clone()
        }
        async fn plan(&self, cx: &Cx, spec: &Spec) -> buckle_core::Result<Planned> {
            Ok(match cx.system.stat(Path::new(&spec.path)).await? {
                Some(_) => Planned::NoOp,
                None => Planned::change(Diff::Create {
                    summary: format!("create {}", spec.path),
                    preview: None,
                }),
            })
        }
        async fn apply(&self, _cx: &Cx, spec: &Spec, _p: &Planned) -> buckle_core::Result<Applied> {
            Ok(Applied::changed(format!("created {}", spec.path)))
        }
    }

    buckle_core::register_task!(CliTestFile);

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    /// Writes a small configuration tree using the test kind.
    fn write_config(dir: &Path, task_kind: &str) {
        std::fs::write(
            dir.join("buckle.yaml"),
            "version: 2\nfleet:\n  groups:\n    home: 'host.labels.site == \"home\"'\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("modules/alpha")).unwrap();
        std::fs::create_dir_all(dir.join("modules/beta")).unwrap();
        std::fs::write(
            dir.join("modules/alpha/module.yaml"),
            format!(
                "name: alpha\ntasks:\n  - {task_kind}: {{ path: '{}/alpha-{{{{ host.facts.arch }}}}.txt' }}\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("modules/beta/module.yaml"),
            format!(
                "name: beta\nneeds: [alpha]\nwhen: 'host.facts.os.family == \"debian\"'\ntasks:\n  - {task_kind}: {{ path: '{}/beta.txt' }}\n",
                dir.display()
            ),
        )
        .unwrap();
    }

    fn local_args(dir: &Path, output: OutputFormat) -> LocalArgs {
        LocalArgs {
            config: dir.to_path_buf(),
            host_id: Some("test-host".into()),
            labels: parse_labels("site=home,canary=true").unwrap(),
            select: None,
            state_dir: dir.join("state"),
            output,
            facts: Some(fixtures().join("facts/debian-arm64.json")),
        }
    }

    #[rstest]
    #[case("", &[])]
    #[case("a=1", &[("a", "1")])]
    #[case("a=1, b = two", &[("a", "1"), ("b", "two")])]
    fn labels_parse_as_key_value_pairs(#[case] input: &str, #[case] expected: &[(&str, &str)]) {
        let parsed = parse_labels(input).unwrap();
        let expected: BTreeMap<String, String> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn labels_without_equals_are_rejected() {
        assert!(parse_labels("oops").is_err());
    }

    #[rstest]
    #[case("true", serde_json::json!(true))]
    #[case("42", serde_json::json!(42))]
    #[case("worker", serde_json::json!("worker"))]
    #[case("1.5", serde_json::json!(1.5))]
    fn label_values_are_typed_scalars(#[case] raw: &str, #[case] expected: serde_json::Value) {
        assert_eq!(label_value(raw), expected);
    }

    #[tokio::test]
    async fn plan_text_lists_modules_and_the_summary_line() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "cli.test_file");
        let mut out = Vec::new();
        let code = plan_to(
            PlanArgs {
                local: local_args(dir.path(), OutputFormat::Text),
            },
            &mut out,
        )
        .await
        .unwrap_or_else(|e| panic!("{}", e.message()));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "a plan with changes exits 2:\n{text}");
        assert!(text.contains("host  test-host"), "{text}");
        assert!(
            text.contains("facts  os=debian/bookworm arch=aarch64"),
            "{text}"
        );
        assert!(
            text.contains("selected  alpha beta    (2 of 2 modules)"),
            "{text}"
        );
        assert!(text.contains("\nalpha\n"), "{text}");
        assert!(text.contains("beta    needs: alpha"), "{text}");
        assert!(text.contains("alpha-aarch64.txt"), "{text}");
        assert!(
            text.contains(
                "summary  2 modules changed · 2 changes · 0 removals · 0 deferred · plan digest "
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn plan_json_parses_as_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "cli.test_file");
        let mut out = Vec::new();
        let code = plan_to(
            PlanArgs {
                local: local_args(dir.path(), OutputFormat::Json),
            },
            &mut out,
        )
        .await
        .unwrap_or_else(|e| panic!("{}", e.message()));
        assert_eq!(code, 2);
        let plan: Plan = serde_json::from_slice(&out).expect("valid plan JSON");
        let names: Vec<&str> = plan.modules.iter().map(|m| m.module.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(plan.host, "test-host");
        assert_eq!(plan.changes(), 2);
    }

    #[tokio::test]
    async fn plan_exits_zero_when_nothing_would_change() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "cli.test_file");
        std::fs::write(dir.path().join("alpha-aarch64.txt"), "").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "").unwrap();
        let mut out = Vec::new();
        let code = plan_to(
            PlanArgs {
                local: local_args(dir.path(), OutputFormat::Text),
            },
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    }

    #[tokio::test]
    async fn validate_reports_unknown_kinds_and_passes_good_configs() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "no.such_kind");
        let mut out = Vec::new();
        let code = validate_to(
            ValidateArgs {
                config: dir.path().to_path_buf(),
                facts: vec![],
                labels: BTreeMap::new(),
            },
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("no.such_kind"), "{text}");
        assert!(
            text.contains("modules/alpha/module.yaml tasks[0]"),
            "{text}"
        );

        let good = tempfile::tempdir().unwrap();
        write_config(good.path(), "cli.test_file");
        let mut out = Vec::new();
        let code = validate_to(
            ValidateArgs {
                config: good.path().to_path_buf(),
                facts: vec![
                    fixtures().join("facts/debian-arm64.json"),
                    fixtures().join("facts/ubuntu-amd64.json"),
                ],
                labels: BTreeMap::new(),
            },
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("0 errors"), "{text}");
    }

    #[tokio::test]
    async fn validate_applies_labels_to_every_fixture_and_reports_groups_and_indeterminate_modules()
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("buckle.yaml"),
            "version: 2\nfleet:\n  groups:\n    canary: 'host.labels.canary == true'\n    rack3: 'host.labels.rack == 3'\n    named: 'host.labels.name == \"3\"'\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/canaried")).unwrap();
        std::fs::write(
            dir.path().join("modules/canaried/module.yaml"),
            "name: canaried\nwhen: 'host.groups contains \"canary\"'\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/half/files")).unwrap();
        let mut out = Vec::new();
        let code = validate_to(
            ValidateArgs {
                config: dir.path().to_path_buf(),
                facts: vec![
                    fixtures().join("facts/debian-arm64.json"),
                    fixtures().join("facts/ubuntu-amd64.json"),
                ],
                labels: parse_labels("canary=true,rack=3,name=\"3\"").unwrap(),
            },
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            text.matches("groups canary, rack3, named; selected canaried")
                .count(),
            2,
            "labels apply to every fixture, typed: {text}"
        );
        assert!(text.contains("indeterminate: half"), "{text}");
    }

    #[tokio::test]
    async fn plan_refuses_an_invalid_selector_and_a_missing_facts_file() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "cli.test_file");
        let mut args = local_args(dir.path(), OutputFormat::Text);
        args.select = Some("host.labels.role ==".into());
        let err = plan_to(PlanArgs { local: args }, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        assert!(err.message().contains("selector"), "{}", err.message());

        let mut args = local_args(dir.path(), OutputFormat::Text);
        args.facts = Some(dir.path().join("missing.json"));
        let err = plan_to(PlanArgs { local: args }, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    /// Plans the shipped sample configuration with the standard library's kinds.
    /// `sysctl.param` probes the host with the `sysctl` binary, so this only
    /// runs where that exists.
    #[tokio::test]
    #[ignore = "needs the `sysctl` binary on PATH (sysctl.param probes the live value)"]
    async fn plan_renders_the_minimal_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = local_args(&fixtures().join("config/minimal"), OutputFormat::Text);
        args.state_dir = dir.path().join("state");
        let mut out = Vec::new();
        let code = plan_to(PlanArgs { local: args }, &mut out)
            .await
            .unwrap_or_else(|e| panic!("{}", e.message()));
        let text = String::from_utf8(out).unwrap();
        assert!(code == 0 || code == 2, "{text}");
        assert!(text.contains("\nmotd\n"), "{text}");
        assert!(text.contains("sysctl    needs: motd"), "{text}");
        assert!(text.contains("summary  "), "{text}");
    }

    fn status_args(dir: &Path) -> StatusArgs {
        StatusArgs {
            config: None,
            state_dir: dir.to_path_buf(),
            output: OutputFormat::Text,
        }
    }

    #[test]
    fn status_without_a_recorded_run_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        assert_eq!(status_to(status_args(dir.path()), &mut out).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no run is recorded"), "{text}");
    }

    #[test]
    fn status_prints_the_last_runs_report_from_the_state_store() {
        use buckle_core::engine::{
            ApplyOutcome, ModuleOutcome, ModuleStatus, StepOutcome, StepState,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(dir.path().join("state.cbor"));
        let mut state = buckle_core::state::State::default();
        state.push_run(
            buckle_core::state::RunRecord {
                revision: RevisionId::new("working-tree"),
                plan_digest: buckle_core::Digest::of(b"p"),
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                outcome: buckle_core::state::RunOutcome::Failed,
                changes: 0,
                removals: 0,
                failed_module: Some("swap".into()),
                rolled_back_modules: vec![],
                error: Some("module 'swap': mkswap failed".into()),
            },
            50,
        );
        store.save(&state).unwrap();
        let failed = StepOutcome {
            id: "command.run[mkswap /swap]".into(),
            kind: "command.run".into(),
            name: None,
            summary: "run `mkswap /swap`".into(),
            state: StepState::Failed,
            result: None,
            started_at: None,
            finished_at: None,
            duration_ms: Some(30),
            error_detail: Some(
                buckle_core::error::ErrorReport::user("mkswap failed")
                    .with_causes(["exit status 1"])
                    .with_advice(["Check the size of /swap."]),
            ),
        };
        store
            .save_last_run(&ApplyOutcome {
                plan_digest: buckle_core::Digest::of(b"p"),
                succeeded: false,
                changes: 0,
                removals: 0,
                duration_ms: 30,
                module_results: vec![ModuleOutcome {
                    steps: vec![failed],
                    ..ModuleOutcome::new("swap", ModuleStatus::Failed)
                }],
                failed_module: Some("swap".into()),
                rolled_back_modules: vec![],
                not_started: vec![],
                health: None,
                error: Some("module 'swap': mkswap failed".into()),
                error_detail: None,
                removals_run: vec![],
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        let mut out = Vec::new();
        assert_eq!(status_to(status_args(dir.path()), &mut out).unwrap(), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("last run  revision working-tree"),
            "{text}"
        );
        assert!(text.contains("· failed"), "{text}");
        assert!(text.contains("swap  FAILED"), "{text}");
        assert!(text.contains("  ✗ command.run[mkswap /swap]"), "{text}");
        assert!(text.contains("      caused by: exit status 1\n      to fix:\n        • Check the size of /swap.\n"), "{text}");
    }
}
