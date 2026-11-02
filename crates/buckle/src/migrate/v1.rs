//! Reads a Buckle 1.x configuration directory into plain data.
//!
//! The layout and the quirks below are those of the v1 loader (`src/core` on
//! `origin/main`): `config/` and `secrets/` hold `KEY=value` files and scripts,
//! `packages/<id>/package.yml` describes a package, `files/<group>/…` holds the
//! files (`.tpl` ones are Go templates) and `scripts/` holds the scripts that
//! run after the files are placed.

use buckle_core::error;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Everything read from a v1 directory.
#[derive(Debug, Default)]
pub struct V1 {
    pub config: Scope,
    pub secrets: Scope,
    pub packages: Vec<Package>,
    /// Entries the v1 loader never looked at; reported so nothing vanishes silently.
    pub ignored: Vec<PathBuf>,
}

/// The `config/` or `secrets/` directory of the root or of a package.
#[derive(Debug, Default)]
pub struct Scope {
    pub env_files: Vec<EnvFile>,
    /// `.sh`, `.ps1`, `.bat` and `.cmd` files whose stdout v1 parsed as `KEY=value` lines.
    pub scripts: Vec<DynamicScript>,
}

#[derive(Debug)]
pub struct EnvFile {
    /// The file name, e.g. `defaults.env`.
    pub name: String,
    /// In file order; a repeated key is kept so the emitter can see the last one win.
    pub entries: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct DynamicScript {
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Debug)]
pub struct Retry {
    pub limit: u32,
    pub delay_ms: u64,
}

#[derive(Debug)]
pub struct Package {
    pub id: String,
    pub description: String,
    pub needs: Vec<String>,
    /// File group name to target directory, as written in `package.yml`.
    pub file_targets: BTreeMap<String, String>,
    pub retry: Option<Retry>,
    /// `package.yml` keys v1 did not read (it ignored them silently).
    pub unknown_keys: Vec<(String, serde_json::Value)>,
    pub config: Scope,
    pub secrets: Scope,
    pub files: Vec<File>,
    pub scripts: Vec<Script>,
}

#[derive(Debug)]
pub struct File {
    pub group: String,
    /// Path inside the group with forward slashes; a `.tpl` suffix is already removed.
    pub rel: String,
    pub is_template: bool,
    /// Executable by someone in the v1 source tree (v1 copied permission bits).
    pub executable: bool,
    pub contents: Vec<u8>,
}

#[derive(Debug)]
pub struct Script {
    pub name: String,
    pub interpreter: &'static str,
    pub contents: Vec<u8>,
}

/// The interpreter v1 used for a script extension.
pub fn interpreter_for(extension: &str) -> Option<&'static str> {
    match extension {
        "sh" => Some("bash"),
        "ps1" => Some("pwsh"),
        "bat" | "cmd" => Some("cmd.exe"),
        _ => None,
    }
}

fn io<T>(result: std::io::Result<T>, doing: &str, path: &Path) -> error::Result<T> {
    result.map_err(|e| {
        error::wrap_user(
            e,
            format!("Could not {doing} {}.", path.display()),
            &["Check that the path exists and that you can read it."],
        )
    })
}

/// Directory entries sorted by name; v1 used directory order, which is not stable.
fn entries(dir: &Path) -> error::Result<Vec<std::fs::DirEntry>> {
    let mut all: Vec<_> = io(std::fs::read_dir(dir), "read the directory", dir)?
        .filter_map(|e| e.ok())
        .collect();
    all.sort_by_key(|e| e.file_name());
    Ok(all)
}

fn name_of(entry: &std::fs::DirEntry) -> String {
    entry.file_name().to_string_lossy().into_owned()
}

/// Parses a v1 `.env` file exactly as v1 did: lines are trimmed and split at the
/// first `=`; lines without one are skipped.
pub fn parse_env(content: &str) -> Vec<(String, String)> {
    content
        .split_terminator('\n')
        .map(str::trim)
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn read_scope(dir: &Path, what: &str) -> error::Result<Scope> {
    let mut scope = Scope::default();
    if !dir.is_dir() {
        return Ok(scope);
    }
    let mut unsupported = Vec::new();
    for entry in entries(dir)? {
        // v1 only looked at files here, and failed on any it could not interpret.
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = name_of(&entry);
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("env") => {
                let text = io(std::fs::read_to_string(&path), "read", &path)?.replace("\r\n", "\n");
                scope.env_files.push(EnvFile {
                    name,
                    entries: parse_env(&text),
                });
            }
            Some(ext) if interpreter_for(ext).is_some() => {
                let contents = io(std::fs::read(&path), "read", &path)?;
                scope.scripts.push(DynamicScript { name, contents });
            }
            _ => unsupported.push(name),
        }
    }
    if unsupported.is_empty() {
        Ok(scope)
    } else {
        Err(error::user(
            format!(
                "The {what} directory {} holds files that Buckle 1.x could not load either: {}.",
                dir.display(),
                unsupported.join(", ")
            ),
            &[
                "v1 only understood .env, .sh, .ps1, .bat and .cmd files here.",
                "Remove or rename them, then run the migration again.",
            ],
        ))
    }
}

fn read_package(dir: &Path, ignored: &mut Vec<PathBuf>) -> error::Result<Package> {
    let id = name_of_dir(dir);
    let yaml_path = dir.join("package.yml");
    let text = std::fs::read_to_string(&yaml_path).map_err(|e| {
        error::wrap_user(
            e,
            format!("The package '{id}' has no readable package.yml."),
            &["A v1 package is a directory containing package.yml; remove the directory if it is not one."],
        )
    })?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| {
        error::wrap_user(
            e,
            format!("The package.yml of '{id}' is not valid YAML."),
            &["Fix the syntax error the inner message points at, then migrate again."],
        )
    })?;
    let doc = match doc {
        serde_yaml::Value::Mapping(map) => map,
        serde_yaml::Value::Null => Default::default(),
        _ => {
            return Err(error::user(
                format!("The package.yml of '{id}' is not a mapping."),
                &["v1 expects `description`, and optionally `needs`, `files` and `retry`."],
            ));
        }
    };

    let mut package = Package {
        id: id.clone(),
        description: String::new(),
        needs: Vec::new(),
        file_targets: BTreeMap::new(),
        retry: None,
        unknown_keys: Vec::new(),
        config: Scope::default(),
        secrets: Scope::default(),
        files: Vec::new(),
        scripts: Vec::new(),
    };
    for (key, value) in doc {
        let key = key.as_str().unwrap_or_default().to_owned();
        let bad = |what: &str| {
            error::user(
                format!("The package.yml of '{id}' has an invalid `{key}`: {what}."),
                &["Compare it with the v1 documentation for package.yml."],
            )
        };
        match key.as_str() {
            "description" => {
                package.description = value
                    .as_str()
                    .ok_or_else(|| bad("expected text"))?
                    .to_owned()
            }
            "needs" => {
                package.needs = serde_yaml::from_value(value)
                    .map_err(|_| bad("expected a list of package names"))?
            }
            "files" => {
                package.file_targets = serde_yaml::from_value(value)
                    .map_err(|_| bad("expected a map of file groups to target directories"))?
            }
            "retry" => {
                #[derive(serde::Deserialize)]
                struct RawRetry {
                    #[serde(default)]
                    limit: u32,
                    // v1 stored the delay in milliseconds (default 5000).
                    #[serde(default = "default_delay")]
                    delay: u64,
                }
                fn default_delay() -> u64 {
                    5000
                }
                let raw: RawRetry = serde_yaml::from_value(value)
                    .map_err(|_| bad("expected `limit` and `delay` (milliseconds)"))?;
                package.retry = Some(Retry {
                    limit: raw.limit,
                    delay_ms: raw.delay,
                });
            }
            _ => {
                let json = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                package.unknown_keys.push((key, json));
            }
        }
    }

    package.config = read_scope(&dir.join("config"), "package config")?;
    package.secrets = read_scope(&dir.join("secrets"), "package secrets")?;
    package.files = read_files(&dir.join("files"), ignored)?;
    package.scripts = read_scripts(&dir.join("scripts"), ignored)?;

    for entry in entries(dir)? {
        let name = name_of(&entry);
        if !matches!(
            name.as_str(),
            "package.yml" | "config" | "secrets" | "files" | "scripts"
        ) {
            ignored.push(entry.path());
        }
    }
    Ok(package)
}

fn name_of_dir(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_files(dir: &Path, ignored: &mut Vec<PathBuf>) -> error::Result<Vec<File>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for group in entries(dir)? {
        if !group.path().is_dir() {
            // v1 only treated sub-directories of files/ as groups.
            ignored.push(group.path());
            continue;
        }
        let group_name = name_of(&group);
        let mut stack = vec![group.path()];
        let mut found = Vec::new();
        while let Some(current) = stack.pop() {
            for entry in entries(&current)? {
                let path = entry.path();
                // v1 followed links; `is_dir`/`is_file` do too.
                if path.is_dir() {
                    stack.push(path);
                } else if path.is_file() {
                    found.push(path);
                }
            }
        }
        found.sort();
        for path in found {
            let rel = path
                .strip_prefix(group.path())
                .map_err(|_| error::system("A file escaped its group directory.", &[]))?
                .to_string_lossy()
                .replace('\\', "/");
            let is_template = rel.ends_with(".tpl");
            let rel = if is_template {
                rel.trim_end_matches(".tpl").to_owned()
            } else {
                rel
            };
            files.push(File {
                group: group_name.clone(),
                rel,
                is_template,
                executable: is_executable(&path),
                contents: io(std::fs::read(&path), "read", &path)?,
            });
        }
    }
    Ok(files)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

fn read_scripts(dir: &Path, ignored: &mut Vec<PathBuf>) -> error::Result<Vec<Script>> {
    let mut scripts = Vec::new();
    if !dir.is_dir() {
        return Ok(scripts);
    }
    let mut unsupported = Vec::new();
    for entry in entries(dir)? {
        let path = entry.path();
        if !path.is_file() {
            ignored.push(path);
            continue;
        }
        let name = name_of(&entry);
        match path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(interpreter_for)
        {
            Some(interpreter) => scripts.push(Script {
                name,
                interpreter,
                contents: io(std::fs::read(&path), "read", &path)?,
            }),
            None => unsupported.push(name),
        }
    }
    if unsupported.is_empty() {
        Ok(scripts)
    } else {
        Err(error::user(
            format!(
                "The scripts directory {} holds files that Buckle 1.x could not run either: {}.",
                dir.display(),
                unsupported.join(", ")
            ),
            &[
                "v1 ran .sh (bash), .ps1 (pwsh), .bat and .cmd (cmd.exe) scripts and failed on anything else.",
                "Remove or rename them, then run the migration again.",
            ],
        ))
    }
}

/// Reads a v1 configuration directory.
pub fn read(dir: &Path) -> error::Result<V1> {
    if !dir.is_dir() {
        return Err(error::user(
            format!("{} is not a directory.", dir.display()),
            &["Pass the Buckle 1.x configuration directory (the one you gave `--config`)."],
        ));
    }
    let packages_dir = dir.join("packages");
    if !packages_dir.is_dir() {
        return Err(error::user(
            format!(
                "{} does not look like a Buckle 1.x configuration: it has no packages/ directory.",
                dir.display()
            ),
            &["Pass the directory that contains packages/, config/ and secrets/."],
        ));
    }
    let mut v1 = V1 {
        config: read_scope(&dir.join("config"), "config")?,
        secrets: read_scope(&dir.join("secrets"), "secrets")?,
        ..Default::default()
    };
    for entry in entries(&packages_dir)? {
        if entry.path().is_dir() {
            let package = read_package(&entry.path(), &mut v1.ignored)?;
            v1.packages.push(package);
        } else {
            // v1 only loaded directories from packages/.
            v1.ignored.push(entry.path());
        }
    }
    for entry in entries(dir)? {
        if !matches!(name_of(&entry).as_str(), "config" | "secrets" | "packages") {
            v1.ignored.push(entry.path());
        }
    }

    v1.ignored.sort();

    let known: Vec<&str> = v1.packages.iter().map(|p| p.id.as_str()).collect();
    for package in &v1.packages {
        for need in &package.needs {
            if !known.contains(&need.as_str()) {
                return Err(error::user(
                    format!(
                        "The package '{}' needs '{need}', which does not exist.",
                        package.id
                    ),
                    &[
                        "Buckle 1.x refused to run this configuration too; fix the `needs` list first.",
                    ],
                ));
            }
        }
    }
    if let Some(cycle) = find_cycle(&v1.packages) {
        return Err(error::user(
            format!(
                "The package dependencies form a cycle: {}.",
                cycle.join(" -> ")
            ),
            &["Remove one of the `needs` entries."],
        ));
    }
    Ok(v1)
}

/// A dependency cycle among the packages, if there is one.
fn find_cycle(packages: &[Package]) -> Option<Vec<String>> {
    fn visit<'a>(
        id: &'a str,
        by_id: &BTreeMap<&'a str, &'a Package>,
        path: &mut Vec<&'a str>,
        done: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        if done.contains(&id) {
            return None;
        }
        if let Some(at) = path.iter().position(|p| *p == id) {
            let mut cycle: Vec<String> = path[at..].iter().map(|s| s.to_string()).collect();
            cycle.push(id.to_owned());
            return Some(cycle);
        }
        path.push(id);
        for need in &by_id.get(id)?.needs {
            if let Some(cycle) = visit(need, by_id, path, done) {
                return Some(cycle);
            }
        }
        path.pop();
        done.push(id);
        None
    }
    let by_id: BTreeMap<&str, &Package> = packages.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut done = Vec::new();
    for package in packages {
        if let Some(cycle) = visit(&package.id, &by_id, &mut Vec::new(), &mut done) {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("A=1\n", &[("A", "1")])]
    #[case("A=1\r\nB=two\r\n", &[("A", "1"), ("B", "two")])]
    #[case("C=x=y\n", &[("C", "x=y")])]
    #[case("not a pair\n\nA=1\n", &[("A", "1")])]
    #[case("  A = 1 \n", &[("A ", " 1")])]
    #[case("A=1\nA=2\n", &[("A", "1"), ("A", "2")])]
    fn env_files_parse_like_v1(#[case] text: &str, #[case] expected: &[(&str, &str)]) {
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(parse_env(text), expected);
    }

    #[rstest]
    #[case("sh", Some("bash"))]
    #[case("ps1", Some("pwsh"))]
    #[case("bat", Some("cmd.exe"))]
    #[case("cmd", Some("cmd.exe"))]
    #[case("py", None)]
    fn interpreters_match_v1(#[case] ext: &str, #[case] expected: Option<&str>) {
        assert_eq!(interpreter_for(ext), expected);
    }
}
