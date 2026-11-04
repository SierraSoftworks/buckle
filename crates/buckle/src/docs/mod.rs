//! The generated half of the documentation site (`docs/`): the command-line
//! reference read from the clap definition and the task, fact and health
//! reference read from the registries, written to
//! `docs/.vuepress/data/reference.json` for the site's Vue components.
//!
//! The tests fail when the committed file no longer matches this binary, or
//! when a kind or command has no section in the site. Regenerate with:
//!
//! ```sh
//! BUCKLE_BLESS_DOCS=1 cargo test -p buckle docs::
//! ```

mod schema;

use clap::CommandFactory;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
struct Reference {
    cli: Cli,
    tasks: Vec<Kind>,
    facts: Vec<Kind>,
    health: Vec<Kind>,
}

#[derive(Serialize)]
struct Cli {
    about: String,
    global: Vec<Arg>,
    commands: Vec<Command>,
}

#[derive(Serialize)]
struct Command {
    /// `install agent` for a nested subcommand.
    path: String,
    about: String,
    usage: String,
    args: Vec<Arg>,
}

#[derive(Serialize)]
struct Arg {
    /// `--config` / `-c` as written on the command line, or `<V1_DIR>`.
    flag: String,
    short: Option<String>,
    value: Option<String>,
    help: String,
    env: Option<String>,
    default: Option<String>,
    options: Vec<String>,
    required: bool,
    repeatable: bool,
}

#[derive(Serialize)]
struct Kind {
    kind: String,
    description: String,
    platforms: Vec<String>,
    locks: Vec<String>,
    removal_default: Option<String>,
    removal_caution: bool,
    /// The spec's own documentation, when it says more than `description`.
    details: Option<String>,
    /// For a spec without named fields: what it takes instead.
    shape: Option<String>,
    fields: Vec<schema::Field>,
}

fn reference() -> Reference {
    let catalog = crate::commands::catalog::build();
    let kinds = |list: Vec<buckle_api::KindDescriptor>| -> Vec<Kind> {
        list.into_iter()
            // `cli.*` kinds are registered by this crate's own tests.
            .filter(|k| !k.kind.starts_with("cli."))
            .map(|k| {
                let fields = schema::fields(&k.schema);
                Kind {
                    details: schema::description(&k.schema).filter(|d| *d != k.description),
                    shape: (!k.schema.is_null() && fields.is_empty())
                        .then(|| schema::describe(&k.schema)),
                    kind: k.kind,
                    description: k.description,
                    platforms: k.platforms,
                    locks: k.locks,
                    removal_default: k.removal_default,
                    removal_caution: k.removal_caution,
                    fields,
                }
            })
            .collect()
    };
    Reference {
        cli: cli(),
        tasks: kinds(catalog.tasks),
        facts: kinds(catalog.facts),
        health: kinds(catalog.health),
    }
}

fn cli() -> Cli {
    let mut root = crate::commands::Cli::command();
    root.build();
    let global = root
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(arg)
        .collect();
    let mut commands = Vec::new();
    walk(&root, "", &mut commands);
    Cli {
        about: root.get_about().map(|s| s.to_string()).unwrap_or_default(),
        global,
        commands,
    }
}

fn walk(parent: &clap::Command, prefix: &str, out: &mut Vec<Command>) {
    for sub in parent.get_subcommands() {
        if sub.get_name() == "help" || sub.is_hide_set() {
            continue;
        }
        let path = format!("{prefix}{}", sub.get_name()).trim().to_string();
        // `install` only groups `install agent`: document the leaves.
        if sub.has_subcommands() {
            walk(sub, &format!("{path} "), out);
            continue;
        }
        let mut usage_cmd = sub.clone();
        let usage = usage_cmd
            .render_usage()
            .to_string()
            .trim_start_matches("Usage:")
            .trim()
            .to_string();
        out.push(Command {
            path,
            about: sub
                .get_long_about()
                .or(sub.get_about())
                .map(|s| s.to_string())
                .unwrap_or_default(),
            usage,
            args: sub
                .get_arguments()
                .filter(|a| !a.is_global_set() && !a.is_hide_set())
                .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
                .map(arg)
                .collect(),
        });
    }
}

fn arg(a: &clap::Arg) -> Arg {
    let value = a
        .get_value_names()
        .and_then(|v| v.first())
        .map(|v| v.to_string())
        .filter(|_| a.get_num_args().is_none_or(|n| n.takes_values()))
        .or_else(|| {
            matches!(
                a.get_action(),
                clap::ArgAction::Set | clap::ArgAction::Append
            )
            .then(|| a.get_id().as_str().to_uppercase())
        });
    let flag = match (a.get_long(), a.is_positional()) {
        (Some(long), _) => format!("--{long}"),
        (None, true) => format!("<{}>", value.clone().unwrap_or_default()),
        (None, false) => a.get_short().map(|s| format!("-{s}")).unwrap_or_default(),
    };
    Arg {
        flag,
        short: a.get_short().map(|s| format!("-{s}")),
        value: value.filter(|_| !a.is_positional()),
        help: a
            .get_long_help()
            .or(a.get_help())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        env: a.get_env().map(|e| e.to_string_lossy().into_owned()),
        default: Some(
            a.get_default_values()
                .iter()
                .filter(|_| !matches!(a.get_action(), clap::ArgAction::SetTrue))
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(","),
        )
        .filter(|d| !d.is_empty()),
        options: a
            .get_possible_values()
            .iter()
            .filter(|p| !p.is_hide_set())
            .map(|p| p.get_name().to_string())
            .collect(),
        required: a.is_required_set(),
        repeatable: matches!(a.get_action(), clap::ArgAction::Append),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn docs_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
    }

    fn generated() -> Value {
        serde_json::to_value(reference()).expect("the reference serialises")
    }

    /// Every Markdown page of the site, concatenated.
    fn pages() -> String {
        fn visit(dir: &Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).expect("docs/ is readable").flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') || name == "node_modules" {
                    continue;
                }
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().is_some_and(|e| e == "md") {
                    out.push_str(&std::fs::read_to_string(&path).expect("page is readable"));
                }
            }
        }
        let mut out = String::new();
        visit(&docs_dir(), &mut out);
        out
    }

    // Defaults such as the state directory differ per platform; the site
    // documents the Linux build.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the site documents the Linux build"
    )]
    fn the_site_reference_matches_this_binary() {
        let path = docs_dir().join(".vuepress/data/reference.json");
        let current = generated();
        if std::env::var_os("BUCKLE_BLESS_DOCS").is_some() {
            let text = serde_json::to_string_pretty(&current).unwrap() + "\n";
            std::fs::write(&path, text).expect("reference.json is writable");
            return;
        }
        let committed: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        assert!(
            committed == current,
            "{} is out of date; regenerate it with `BUCKLE_BLESS_DOCS=1 cargo test -p buckle docs::`",
            path.display()
        );
    }

    #[test]
    fn every_kind_and_command_has_a_section_in_the_site() {
        let pages = pages();
        let reference = reference();
        let mut missing = Vec::new();
        for (registry, kinds) in [
            ("tasks", &reference.tasks),
            ("facts", &reference.facts),
            ("health", &reference.health),
        ] {
            for k in kinds {
                let tag = format!(r#"<KindReference registry="{registry}" kind="{}""#, k.kind);
                if !pages.contains(&tag) {
                    missing.push(tag);
                }
            }
        }
        for c in &reference.cli.commands {
            let tag = format!(r#"<CliCommand command="{}""#, c.path);
            if !pages.contains(&tag) {
                missing.push(tag);
            }
        }
        assert!(
            missing.is_empty(),
            "these have no section in docs/: {missing:#?}"
        );
    }

    #[test]
    fn commands_are_listed_with_their_flags() {
        let cli = cli();
        let plan = cli.commands.iter().find(|c| c.path == "plan").unwrap();
        let config = plan.args.iter().find(|a| a.flag == "--config").unwrap();
        assert_eq!(config.short.as_deref(), Some("-c"));
        assert_eq!(config.env.as_deref(), Some("BUCKLE_CONFIG"));
        assert!(config.required);
        assert!(cli.commands.iter().any(|c| c.path == "up"));
        // The old spelling still works but is not advertised.
        assert!(!cli.commands.iter().any(|c| c.path.starts_with("install")));
        assert!(cli.global.iter().any(|a| a.flag == "--verbose"));
        let migrate = cli.commands.iter().find(|c| c.path == "migrate").unwrap();
        assert!(migrate.args.iter().any(|a| a.flag == "<V1_DIR>"));
    }
}
