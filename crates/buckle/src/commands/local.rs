//! `buckle plan|apply|health|facts|validate`: the engine against a local
//! configuration directory. Lane 1 completes these once the engine exists; the
//! argument surface is fixed here.

use clap::Args;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

    /// Labels for this host, as comma-separated key=value pairs.
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
}

impl std::ops::Deref for PlanArgs {
    type Target = LocalArgs;
    fn deref(&self) -> &LocalArgs {
        &self.local
    }
}

pub async fn plan(args: PlanArgs) -> buckle_core::Result<i32> {
    let _ = args;
    Err(super::not_yet("buckle plan"))
}

pub async fn apply(args: ApplyArgs) -> buckle_core::Result<i32> {
    let _ = args;
    Err(super::not_yet("buckle apply"))
}

pub async fn health(args: HealthArgs) -> buckle_core::Result<i32> {
    let _ = args;
    Err(super::not_yet("buckle health"))
}

pub async fn facts(args: FactsArgs) -> buckle_core::Result<i32> {
    let _ = args;
    Err(super::not_yet("buckle facts"))
}

pub async fn validate(args: ValidateArgs) -> buckle_core::Result<i32> {
    let revision = buckle_core::engine::loader::load_dir(
        &args.config,
        buckle_core::plan::RevisionId::new("working-tree"),
    )?;
    println!(
        "{} modules loaded from {}",
        revision.modules.len(),
        args.config.display()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

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
}
