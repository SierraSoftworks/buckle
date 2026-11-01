use std::path::Path;

use tempfile::TempDir;

/// A buckle configuration directory assembled in a temporary folder.
///
/// Tests use this to describe exactly the packages, config files and scripts
/// a scenario needs, rather than sharing the fixtures under `src/test/data`.
pub struct ConfigDir {
    dir: TempDir,
}

impl ConfigDir {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a temporary directory should be created"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Adds a top-level config file (`config/<name>`).
    pub fn with_config(self, name: &str, content: &str) -> Self {
        self.write(&self.path().join("config").join(name), content);
        self
    }

    /// Adds a top-level secrets file (`secrets/<name>`).
    pub fn with_secret(self, name: &str, content: &str) -> Self {
        self.write(&self.path().join("secrets").join(name), content);
        self
    }

    /// Adds a package with the given `package.yml` content.
    pub fn with_package(self, id: &str, package_yml: &str) -> Self {
        self.write(&self.package_path(id).join("package.yml"), package_yml);
        self
    }

    /// Adds a task script to a package (`packages/<id>/scripts/<name>`).
    pub fn with_package_script(self, id: &str, name: &str, content: &str) -> Self {
        self.write(&self.package_path(id).join("scripts").join(name), content);
        self
    }

    /// Adds a config file to a package (`packages/<id>/config/<name>`).
    pub fn with_package_config(self, id: &str, name: &str, content: &str) -> Self {
        self.write(&self.package_path(id).join("config").join(name), content);
        self
    }

    /// Adds a file to one of a package's file groups
    /// (`packages/<id>/files/<group>/<relative>`).
    pub fn with_package_file(self, id: &str, group: &str, relative: &str, content: &str) -> Self {
        self.write(
            &self.package_path(id).join("files").join(group).join(relative),
            content,
        );
        self
    }

    fn package_path(&self, id: &str) -> std::path::PathBuf {
        self.path().join("packages").join(id)
    }

    fn write(&self, path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory should be created");
        }
        std::fs::write(path, content).expect("the file should be written");
    }
}

impl Default for ConfigDir {
    fn default() -> Self {
        Self::new()
    }
}
