//! Turns the data read from a v1 directory into the files of a v2 repository.
//!
//! Nothing here touches the disk: the result is a map of relative paths to
//! contents, so it can be compared with what is already in an output directory
//! and written (or not) as one unit.
//!
//! The mapping, v1 to v2:
//!
//! | v1 | v2 |
//! |---|---|
//! | `config/*.env` | an overlay file `vars/<name>.yaml`, listed in `buckle.yaml` in file-name order |
//! | `packages/<id>/config/*.env` | the module's `vars:` |
//! | `secrets/*.env`, package `secrets/*.env` | a `secrets:` reference per key (values are never copied) |
//! | `package.yml` `description`, `needs` | the module's `description`, `needs` |
//! | `files/<group>/…` | `file.copy` to the group's target directory |
//! | `files/<group>/….tpl` | `file.template` with a converted `.j2` |
//! | `scripts/*` | `file.copy` + `command.run` (v1 ran every script on every apply) |
//! | `retry` | `retry:` on the module's `command.run` tasks |
//! | config and secret scripts | copied to `facts.d/` and `secrets.d/` for a person to rewrite |
//!
//! Anything that needs a decision gets a `TODO(migrate)` comment where it applies.

use super::gotmpl;
use super::v1::{Package, Scope, V1};
use buckle_core::error;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Directory (on the host) that migrated scripts are copied to before they run.
const HOST_SCRIPT_DIR: &str = "/var/lib/buckle/scripts";

pub struct Emitted {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

/// A double-quoted YAML scalar. JSON string syntax is valid YAML.
fn q(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serialises")
}

/// A mapping key: plain when it is a safe identifier, quoted otherwise.
fn key(k: &str) -> String {
    let plain = k
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(
            k.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain { k.to_owned() } else { q(k) }
}

/// Key/value pairs of a scope's env files, later files and later lines winning,
/// as in v1 (whose "later" depended on directory order; here it is name order).
struct Merged {
    values: BTreeMap<String, String>,
    /// Keys set by more than one file or line with different values.
    repeated: BTreeSet<String>,
}

fn merge(scope: &Scope) -> Merged {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut repeated = BTreeSet::new();
    for file in &scope.env_files {
        for (k, v) in &file.entries {
            let k = k.trim().to_owned();
            if values.get(&k).is_some_and(|old| old != v) {
                repeated.insert(k.clone());
            }
            values.insert(k, v.clone());
        }
    }
    Merged { values, repeated }
}

fn secret_keys(scope: &Scope) -> BTreeSet<String> {
    scope
        .env_files
        .iter()
        .flat_map(|f| f.entries.iter().map(|(k, _)| k.trim().to_owned()))
        .collect()
}

/// Comment lines for a TODO, one `# TODO(migrate): …` per line of text.
fn todo(indent: &str, text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let lead = if i == 0 { "TODO(migrate): " } else { "    " };
        let _ = writeln!(out, "{indent}# {lead}{line}");
    }
    out
}

pub fn emit(v1: &V1, v1_dir: &Path) -> error::Result<Emitted> {
    let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut put = |path: String, contents: Vec<u8>| {
        files.insert(PathBuf::from(path), contents);
    };

    let global = merge(&v1.config);
    let global_secrets = secret_keys(&v1.secrets);

    // ---- vars overlays from the global config -------------------------------
    let mut overlay_files = Vec::new();
    for file in &v1.config.env_files {
        let stem = file.name.trim_end_matches(".env");
        let path = format!("vars/{stem}.yaml");
        let mut text = format!(
            "# Migrated from config/{}. Values are strings, as in v1.\n",
            file.name
        );
        let mut seen = BTreeSet::new();
        let mut body = String::new();
        // The last occurrence of a key in the file wins, as in v1.
        let last: BTreeMap<String, &String> = file
            .entries
            .iter()
            .map(|(k, v)| (k.trim().to_owned(), v))
            .collect();
        for (raw, value) in &last {
            let name = gotmpl::identifier(raw);
            if !seen.insert(name.clone()) {
                text.push_str(&todo("", &format!("`{raw}` and another key in this file both became `{name}`; check which value you want.")));
            }
            if &name != raw {
                text.push_str(&todo(
                    "",
                    &format!(
                        "`{raw}` was renamed to `{name}`: template names must be identifiers."
                    ),
                ));
            }
            let _ = writeln!(body, "{}: {}", key(&name), q(value));
        }
        let repeats = file
            .entries
            .iter()
            .filter(|(k, v)| last.get(k.trim()).is_some_and(|l| *l != v))
            .map(|(k, _)| k.trim().to_owned())
            .collect::<BTreeSet<_>>();
        for raw in repeats {
            text.push_str(&todo(
                "",
                &format!(
                    "`{raw}` is set more than once in this file; the last value is kept, as in v1."
                ),
            ));
        }
        if body.is_empty() {
            body.push_str("{}\n");
        }
        text.push_str(&body);
        put(path.clone(), text.into_bytes());
        overlay_files.push(path);
    }
    // Keys that two global files both set: v1 kept whichever it read last.
    let mut cross_file = String::new();
    for raw in &global.repeated {
        let defining: Vec<&str> = v1
            .config
            .env_files
            .iter()
            .filter(|f| f.entries.iter().any(|(k, _)| k.trim() == raw))
            .map(|f| f.name.as_str())
            .collect();
        if defining.len() > 1 {
            cross_file.push_str(&todo(
                "",
                &format!(
                    "`{raw}` is set by {} with different values. v1 kept whichever file it read last (directory order); here the file that sorts last wins.",
                    defining.join(" and ")
                ),
            ));
        }
    }

    // ---- secrets -------------------------------------------------------------
    let mut secret_origins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for k in &global_secrets {
        secret_origins
            .entry(gotmpl::identifier(k))
            .or_default()
            .push("the global secrets/".to_owned());
    }
    for package in &v1.packages {
        for k in secret_keys(&package.secrets) {
            secret_origins
                .entry(gotmpl::identifier(&k))
                .or_default()
                .push(format!("package '{}'", package.id));
        }
    }

    // ---- scripts that produced config or secrets -----------------------------
    let mut script_notes = String::new();
    for script in &v1.config.scripts {
        let path = format!("facts.d/global/{}", script.name);
        script_notes.push_str(&todo("", &format!(
            "config/{0} printed KEY=value lines in v1. It was copied to {path}: rewrite it to print JSON and install it in the agent's facts.d, where its values appear as host.facts.script.*; then point vars and templates at them.",
            script.name
        )));
        put(path, script.contents.clone());
    }
    for script in &v1.secrets.scripts {
        let path = format!("secrets.d/global/{}", script.name);
        script_notes.push_str(&todo("", &format!(
            "secrets/{0} printed KEY=value secrets in v1. It was copied to {path}: declare each secret it produced under `secrets:` below, with an `exec:` provider if the server allows its program.",
            script.name
        )));
        put(path, script.contents.clone());
    }
    for package in &v1.packages {
        for script in &package.config.scripts {
            let path = format!("facts.d/packages/{}/{}", package.id, script.name);
            script_notes.push_str(&todo("", &format!(
                "packages/{}/config/{} printed KEY=value lines in v1. It was copied to {path}: rewrite it to print JSON and install it in the agent's facts.d, then update what used its values.",
                package.id, script.name
            )));
            put(path, script.contents.clone());
        }
        for script in &package.secrets.scripts {
            let path = format!("secrets.d/packages/{}/{}", package.id, script.name);
            script_notes.push_str(&todo("", &format!(
                "packages/{}/secrets/{} printed KEY=value secrets in v1. It was copied to {path}: declare each secret it produced under `secrets:` below.",
                package.id, script.name
            )));
            put(path, script.contents.clone());
        }
    }

    // ---- modules -------------------------------------------------------------
    let mut scoped_overlays = String::new();
    for package in &v1.packages {
        let module = emit_module(
            package,
            &global,
            &global_secrets,
            &secret_keys(&package.secrets),
        )?;
        put(
            format!("modules/{}/module.yaml", package.id),
            module.yaml.into_bytes(),
        );
        for (path, contents) in module.files {
            put(format!("modules/{}/files/{path}", package.id), contents);
        }
        if !module.overrides.is_empty() {
            let _ = writeln!(
                scoped_overlays,
                "  - when: {}",
                q(&format!("module.name == {}", q(&package.id)))
            );
            scoped_overlays.push_str("    values:\n");
            for (name, value) in &module.overrides {
                let _ = writeln!(scoped_overlays, "      {}: {}", key(name), q(value));
            }
        }
    }

    // ---- buckle.yaml -----------------------------------------------------------
    let mut yaml = String::from(
        "# Migrated from a Buckle 1.x configuration by `buckle migrate`.\n\
         # Every `TODO(migrate)` comment in this tree marks a decision a person has to make.\n\n",
    );
    yaml.push_str(&script_notes);
    yaml.push_str(&cross_file);
    if !v1.ignored.is_empty() {
        yaml.push_str("# Not migrated (Buckle 1.x did not read these either):\n");
        for path in &v1.ignored {
            let shown = path.strip_prefix(v1_dir).unwrap_or(path);
            let _ = writeln!(yaml, "#   {}", shown.display());
        }
    }
    yaml.push_str("version: 2\n");
    if !overlay_files.is_empty() || !scoped_overlays.is_empty() {
        yaml.push_str("\n# Overlays apply in order, later winning. Module `vars:` sit below all of them.\nvars:\n");
        for path in &overlay_files {
            let _ = writeln!(yaml, "  - file: {path}");
        }
        if !scoped_overlays.is_empty() {
            yaml.push_str(&todo("  ", "In v1 a package's config overrode global config of the same name; in v2 fleet vars override module vars. These overlays keep the v1 result for the keys involved."));
            yaml.push_str(&scoped_overlays);
        }
    }
    if !secret_origins.is_empty() {
        yaml.push_str("\n# References only: v1 secret values were deliberately not copied.\n");
        yaml.push_str(&todo("", "Say where each secret comes from: `file:` (a path a Nomad template renders), `env:` or `exec:`. The paths below are placeholders; provision a value for each."));
        yaml.push_str("secrets:\n");
        for (name, origins) in &secret_origins {
            if origins.len() > 1 {
                yaml.push_str(&todo("  ", &format!(
                    "v1 defined `{name}` in {}; v2 secrets are fleet-wide, so they now share one value. Split the name if they differed.",
                    origins.join(" and ")
                )));
            }
            let _ = writeln!(
                yaml,
                "  {}: {{ file: {} }}",
                key(name),
                q(&format!("/secrets/{name}"))
            );
        }
    }
    put("buckle.yaml".to_owned(), yaml.into_bytes());

    Ok(Emitted { files })
}

struct Module {
    yaml: String,
    /// Paths relative to the module's `files/` directory.
    files: BTreeMap<String, Vec<u8>>,
    /// Package config keys that also exist globally, as (identifier, value).
    overrides: BTreeMap<String, String>,
}

fn emit_module(
    package: &Package,
    global: &Merged,
    global_secrets: &BTreeSet<String>,
    package_secrets: &BTreeSet<String>,
) -> error::Result<Module> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let own = merge(&package.config);

    // What v1 exposed to this package's templates and scripts: global config,
    // overlaid by package config, overlaid by secrets.
    let secrets: BTreeSet<String> = global_secrets.union(package_secrets).cloned().collect();
    let mut defined: BTreeSet<String> = global.values.keys().cloned().collect();
    defined.extend(own.values.keys().cloned());
    defined.extend(secrets.iter().cloned());

    let mut yaml = format!("name: {}\n", key(&package.id));
    if !package.description.is_empty() {
        let _ = writeln!(yaml, "description: {}", q(package.description.trim_end()));
    }
    if !package.needs.is_empty() {
        let needs: Vec<String> = package.needs.iter().map(|n| q(n)).collect();
        let _ = writeln!(yaml, "needs: [{}]", needs.join(", "));
    }
    if !package.unknown_keys.is_empty() {
        yaml.push_str(&todo(
            "",
            "package.yml had keys Buckle 1.x ignored. They are kept here for reference only:",
        ));
        for (k, v) in &package.unknown_keys {
            let _ = writeln!(yaml, "#   {k}: {v}");
        }
    }

    // vars
    let mut overrides = BTreeMap::new();
    if !own.values.is_empty() {
        yaml.push_str("vars:\n");
        for (raw, value) in &own.values {
            let name = gotmpl::identifier(raw);
            if &name != raw {
                yaml.push_str(&todo(
                    "  ",
                    &format!(
                        "`{raw}` was renamed to `{name}`: template names must be identifiers."
                    ),
                ));
            }
            if own.repeated.contains(raw) {
                yaml.push_str(&todo("  ", &format!("`{raw}` is set to different values by several config files; the file that sorts last wins here (v1 used directory order).")));
            }
            let _ = writeln!(yaml, "  {}: {}", key(&name), q(value));
            if global.values.contains_key(raw) {
                overrides.insert(name, value.clone());
            }
        }
    }

    // tasks
    let mut tasks = String::new();
    let mut dests: BTreeMap<String, usize> = BTreeMap::new();
    for file in &package.files {
        let target = package
            .file_targets
            .get(&file.group)
            .map(String::as_str)
            .unwrap_or("/");
        let dest = Path::new(target)
            .join(&file.rel)
            .to_string_lossy()
            .replace('\\', "/");
        let n = dests.entry(dest.clone()).or_insert(0);
        *n += 1;
        let duplicate = *n > 1;
        if !Path::new(target).has_root() {
            tasks.push_str(&todo("  ", &format!("the target directory for group `{}` is `{target}`, which is not absolute; v1 wrote it relative to its working directory and v2 needs an absolute path.", file.group)));
        }
        if duplicate {
            tasks.push_str(&todo("  ", &format!("another file also writes {dest}. v1 wrote them in name order, so the later one won; it still does here.")));
        }
        let id = if duplicate {
            format!("    id: {}\n", q(&format!("{dest}#{n}")))
        } else {
            String::new()
        };
        if file.is_template {
            let text = String::from_utf8(file.contents.clone()).map_err(|_| {
                error::user(
                    format!(
                        "The template {}/{} in package '{}' is not valid UTF-8.",
                        file.group, file.rel, package.id
                    ),
                    &["Buckle 1.x could not render it either; fix the file and migrate again."],
                )
            })?;
            // v1 rendered templates byte for byte; Jinja output is line based, so
            // Windows checkouts (CRLF) must migrate to the same template as LF ones.
            let text = text.replace("\r\n", "\n");
            let converted = gotmpl::convert(&text, &secrets, &defined);
            let src = format!("{}/{}.j2", file.group, file.rel);
            let contents = if converted.todos.is_empty() {
                converted.text
            } else {
                tasks.push_str(&todo("  ", &format!("{} has Go template syntax that was not converted (see the comment at the top of the template).", src)));
                format!(
                    "{{# TODO(migrate): fix these, then delete this comment:\n{}\n#}}{}",
                    converted
                        .todos
                        .iter()
                        .map(|t| format!("  - {t}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    converted.text
                )
            };
            let _ = write!(
                tasks,
                "  - file.template:\n      src: {}\n      dest: {}\n{id}",
                q(&src),
                q(&dest)
            );
            files.insert(src, contents.into_bytes());
        } else {
            let src = format!("{}/{}", file.group, file.rel);
            let mode = if file.executable {
                "      mode: \"0755\"\n"
            } else {
                ""
            };
            let _ = write!(
                tasks,
                "  - file.copy:\n      src: {}\n      dest: {}\n{mode}{id}",
                q(&src),
                q(&dest)
            );
            files.insert(src, file.contents.clone());
        }
    }

    // Scripts live beside the files; pick a directory no file group already uses.
    let groups: BTreeSet<&str> = package.files.iter().map(|f| f.group.as_str()).collect();
    let mut script_dir = "_scripts".to_owned();
    while groups.contains(script_dir.as_str()) {
        script_dir.push('_');
    }
    let env = script_env(&defined, &secrets);
    for script in &package.scripts {
        let src = format!("{script_dir}/{}", script.name);
        let dest = format!("{HOST_SCRIPT_DIR}/{}/{}", package.id, script.name);
        tasks.push_str(&todo("  ", &format!(
            "v1 ran {} on every apply, after the files. Add `creates:`, `unless:` or `changed_when:` to the command so v2 can plan it as a no-op.",
            script.name
        )));
        if script.interpreter != "bash" {
            tasks.push_str(&todo("  ", &format!(
                "v1 ran this with {}; the destination {dest} is a unix path, so adjust it for the target OS.",
                script.interpreter
            )));
        }
        let _ = write!(
            tasks,
            "  - file.copy:\n      src: {}\n      dest: {}\n      mode: \"0700\"\n",
            q(&src),
            q(&dest)
        );
        files.insert(src, script.contents.clone());
        let _ = write!(
            tasks,
            "  - command.run:\n      run: [{}, {}]\n",
            q(script.interpreter),
            q(&dest)
        );
        if !env.is_empty() {
            tasks.push_str("      env:\n");
            for (name, value) in &env {
                let _ = writeln!(tasks, "        {}: {}", q(name), q(value));
            }
        }
        if let Some(retry) = package.retry.as_ref().filter(|r| r.limit > 0) {
            let _ = writeln!(
                tasks,
                "    retry: {{ limit: {}, delay: {}ms }}",
                retry.limit, retry.delay_ms
            );
        }
    }

    if let Some(retry) = package.retry.as_ref().filter(|r| r.limit > 0) {
        yaml.push_str(&todo("", &format!(
            "v1 retried this whole package up to {} times, {}ms apart. v2 retries individual tasks (`retry: {{ limit, delay }}`); it was set on the commands below, add it to any other task that can fail transiently.",
            retry.limit, retry.delay_ms
        )));
    }
    if tasks.is_empty() {
        yaml.push_str("tasks: []\n");
    } else {
        yaml.push_str("tasks:\n");
        yaml.push_str(&tasks);
    }
    Ok(Module {
        yaml,
        files,
        overrides,
    })
}

/// The environment v1 gave every script of a package: config, overlaid by the
/// package's config, overlaid by secrets, as `NAME: template` pairs.
fn script_env(defined: &BTreeSet<String>, secrets: &BTreeSet<String>) -> BTreeMap<String, String> {
    defined
        .iter()
        .map(|raw| {
            let name = gotmpl::identifier(raw);
            let scope = if secrets.contains(raw) {
                "secrets"
            } else {
                "vars"
            };
            (raw.clone(), format!("{{{{ {scope}.{name} }}}}"))
        })
        .collect()
}
