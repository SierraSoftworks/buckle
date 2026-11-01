use serde::*;
use solvent::DepGraph;
use std::fs;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tracing::instrument;

use crate::errors;

use super::retry::RetryConfig;
use super::Core;
use super::{file::File, script::Script};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    #[serde(skip)]
    pub id: String,
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub files: HashMap<String, PathBuf>,
    #[serde(default)]
    pub retry: RetryConfig,

    #[serde(skip)]
    path: PathBuf,
}

impl Package {
    #[instrument(level = "debug", name = "package.load", err)]
    pub fn load(path: &Path) -> Result<Package, errors::Error> {
        let content = fs::read(path.join("package.yml"))?;
        let mut pkg: Package = serde_yaml::from_slice(&content)?;

        pkg.path = path.to_owned();
        pkg.id = path
            .components()
            .last()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap();

        Ok(pkg)
    }

    pub fn get_tasks(&self) -> Result<Vec<Script>, errors::Error> {
        super::script::get_all_scripts(&self.path.join("scripts"))
    }

    pub fn get_config(&self, core: &Core) -> Result<HashMap<String, String>, errors::Error> {
        super::config::load_all_config(core, &self.path.join("config"))
    }

    pub fn get_secrets(&self, core: &Core) -> Result<HashMap<String, String>, errors::Error> {
        super::config::load_all_config(core, &self.path.join("secrets"))
    }

    pub fn get_files(&self) -> Result<Vec<File>, errors::Error> {
        super::file::get_all_files(&self.path.join("files"))
    }
}

#[instrument(level = "debug", name = "package.load_all", err)]
pub fn get_all_packages(dir: &Path) -> Result<Vec<Package>, errors::Error> {
    let results = dir
        .read_dir()
        .map(|dirs| {
            dirs.filter_map(|dir| match dir {
                Ok(d) => match d.file_type() {
                    Ok(ft) if ft.is_dir() => Some(d.path()),
                    _ => None,
                },
                _ => None,
            })
        })
        .map_err(|err| {
            errors::user_with_internal(
            "Failed to read the list of packages files.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err)
        })?
        .map(|p| Package::load(&p));

    let mut packages_lookup: HashMap<String, Package> = HashMap::new();

    let mut depgraph: DepGraph<&str> = DepGraph::new();

    for result in results {
        let package = result?;

        packages_lookup.insert(package.id.clone(), package);
    }

    for (id, package) in packages_lookup.iter() {
        depgraph.register_node(id);
        depgraph.register_dependencies(id, package.needs.iter().map(|s| s.as_str()).collect());
        depgraph.register_dependency("__complete", id);
    }

    let order = depgraph.dependencies_of(&"__complete").map_err(|e| errors::user_with_internal(
        "Failed to calculate a valid execution graph based on the dependencies specified in your packages.",
        "Make sure that your packages specify valid dependencies and that there are no circular references.",
        e))?;

    let mut packages = Vec::new();

    for node in order {
        let &node = node.map_err(|e| errors::user_with_internal(
            "Failed to calculate a valid execution graph based on the dependencies specified in your packages.",
            "Make sure that your packages specify valid dependencies and that there are no circular references.",
            e))?;

        match node {
            "__complete" => {}
            _ => {
                let package = packages_lookup.get(node).ok_or_else(|| errors::user(
                    &format!("Failed to find package with name '{node}' although it was present in the dependency graph."),
                    "Make sure that this package is present, or remove the dependency from any packages which currently need it."
                ))?;
                packages.push(package.clone());
            }
        }
    }

    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{get_test_data, ConfigDir};

    #[test]
    fn load_reads_the_package_definition() {
        let package = Package::load(&get_test_data().join("packages").join("test1"))
            .expect("the package should load");

        assert_eq!(package.id, "test1");
        assert_eq!(package.description, "The first test package.");
        assert!(package.needs.is_empty());
        assert_eq!(
            package.files.get("conf.d"),
            Some(&PathBuf::from("/etc/test.conf")),
            "file groups should map to their target paths"
        );
        assert_eq!(package.retry.limit, 0, "retries should default to none");
    }

    #[test]
    fn load_fails_without_a_package_file() {
        let dir = ConfigDir::new();
        std::fs::create_dir_all(dir.path().join("packages").join("empty")).unwrap();

        let result = Package::load(&dir.path().join("packages").join("empty"));

        assert!(result.is_err(), "a package without package.yml should not load");
    }

    #[test]
    fn get_all_packages_orders_dependencies_first() {
        let dir = ConfigDir::new()
            .with_package("app", "description: app\nneeds: [runtime, config]\n")
            .with_package("runtime", "description: runtime\nneeds: [base]\n")
            .with_package("config", "description: config\nneeds: [base]\n")
            .with_package("base", "description: base\n");

        let packages = get_all_packages(&dir.path().join("packages"))
            .expect("the packages should load");
        let order: Vec<&str> = packages.iter().map(|p| p.id.as_str()).collect();

        assert_eq!(order.len(), 4, "every package should be included exactly once");
        let position = |id: &str| order.iter().position(|p| *p == id).unwrap();
        assert!(position("base") < position("runtime"));
        assert!(position("base") < position("config"));
        assert!(position("runtime") < position("app"));
        assert!(position("config") < position("app"));
    }

    #[test]
    fn get_all_packages_rejects_dependency_cycles() {
        let dir = ConfigDir::new()
            .with_package("a", "description: a\nneeds: [b]\n")
            .with_package("b", "description: b\nneeds: [a]\n");

        let result = get_all_packages(&dir.path().join("packages"));

        assert!(result.is_err(), "a dependency cycle should be reported");
    }

    #[test]
    fn get_all_packages_rejects_unknown_dependencies() {
        let dir = ConfigDir::new().with_package("a", "description: a\nneeds: [missing]\n");

        let result = get_all_packages(&dir.path().join("packages"));

        assert!(result.is_err(), "a dependency on a missing package should be reported");
    }

    #[test]
    fn get_all_packages_fails_for_a_missing_directory() {
        let dir = ConfigDir::new();

        let result = get_all_packages(&dir.path().join("packages"));

        assert!(result.is_err(), "a missing packages directory should be reported");
    }
}
