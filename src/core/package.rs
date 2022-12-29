use serde::*;
use solvent::DepGraph;
use std::fs;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tracing::instrument;

use crate::errors;

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

    pub fn get_config(&self) -> Result<HashMap<String, String>, errors::Error> {
        super::config::load_all_config(&self.path.join("config"))
    }

    pub fn get_secrets(&self) -> Result<HashMap<String, String>, errors::Error> {
        super::config::load_all_config(&self.path.join("secrets"))
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
