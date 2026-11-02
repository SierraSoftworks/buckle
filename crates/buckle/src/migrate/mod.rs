//! `buckle migrate <v1-dir> <out-dir>`: convert a Buckle 1.x configuration
//! directory into a v2 repository (`buckle.yaml`, `vars/`, `modules/`).
//!
//! The conversion is a pure function from the v1 tree to a set of files
//! ([`convert`]); [`write`] puts them on disk. Running it twice on the same
//! input gives the same files, and an existing, non-empty output directory is
//! never touched unless it already holds exactly what would be written (a
//! no-op) or `--force` was given.

mod emit;
mod gotmpl;
mod v1;

use buckle_core::error::{self, HumanizableResult};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The files a migration would write, by path relative to the output directory.
#[derive(Debug)]
pub struct Migration {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    /// v1 entries that Buckle 1.x never read and that were therefore not migrated.
    pub ignored: Vec<PathBuf>,
}

impl Migration {
    /// How many `TODO(migrate)` comments the output contains.
    pub fn todo_count(&self) -> usize {
        self.files
            .values()
            .map(|bytes| String::from_utf8_lossy(bytes).matches("TODO(migrate)").count())
            .sum()
    }
}

/// What [`write`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    /// The output directory was empty or new.
    Created(usize),
    /// `--force` replaced what was there.
    Replaced(usize),
    /// The output directory already held exactly these files.
    Unchanged,
}

/// Reads `v1_dir` and works out the v2 files, without writing anything.
pub fn convert(v1_dir: &Path) -> error::Result<Migration> {
    let v1 = v1::read(v1_dir)?;
    let emitted = emit::emit(&v1, v1_dir)?;
    Ok(Migration {
        files: emitted.files,
        ignored: v1.ignored,
    })
}

/// The paths inside the output directory that a migration owns, and that
/// `--force` therefore replaces. Anything else in the directory is left alone.
const OWNED: &[&str] = &["buckle.yaml", "vars", "modules", "facts.d", "secrets.d"];

/// Writes a migration into `out_dir`.
pub fn write(
    v1_dir: &Path,
    out_dir: &Path,
    migration: &Migration,
    force: bool,
) -> error::Result<Written> {
    if out_dir.exists() && !out_dir.is_dir() {
        return Err(error::user(
            format!("{} exists and is not a directory.", out_dir.display()),
            &["Choose a directory that does not exist yet, or an empty one."],
        ));
    }
    if let (Ok(v1), Ok(out)) = (v1_dir.canonicalize(), out_dir.canonicalize())
        && (out == v1 || v1.starts_with(&out))
    {
        return Err(error::user(
            "The output directory is, or contains, the v1 directory.",
            &["Write the v2 repository somewhere else so that the v1 tree stays intact."],
        ));
    }

    let occupied = out_dir.is_dir()
        && std::fs::read_dir(out_dir)
            .to_human_error()?
            .next()
            .is_some();
    if occupied && !force {
        let same = migration.files.iter().all(|(rel, bytes)| {
            std::fs::read(out_dir.join(rel)).is_ok_and(|existing| &existing == bytes)
        });
        if same {
            return Ok(Written::Unchanged);
        }
        return Err(error::user(
            format!("{} already contains files.", out_dir.display()),
            &[
                "Choose an empty directory, or pass --force to replace buckle.yaml, vars/, modules/, facts.d/ and secrets.d/ there.",
            ],
        ));
    }
    if occupied {
        for name in OWNED {
            let path = out_dir.join(name);
            if path.is_dir() {
                std::fs::remove_dir_all(&path).to_human_error()?;
            } else if path.exists() {
                std::fs::remove_file(&path).to_human_error()?;
            }
        }
    }
    for (rel, bytes) in &migration.files {
        let path = out_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).to_human_error()?;
        }
        std::fs::write(&path, bytes).to_human_error()?;
    }
    Ok(if occupied {
        Written::Replaced(migration.files.len())
    } else {
        Written::Created(migration.files.len())
    })
}

/// The `buckle migrate` command.
pub fn run(args: crate::commands::MigrateArgs) -> error::Result<i32> {
    let migration = convert(&args.v1_dir)?;
    let written = write(&args.v1_dir, &args.out_dir, &migration, args.force)?;
    match written {
        Written::Unchanged => println!(
            "{} already holds this migration; nothing to do.",
            args.out_dir.display()
        ),
        Written::Created(n) | Written::Replaced(n) => {
            println!("Wrote {n} files to {}.", args.out_dir.display())
        }
    }
    for path in &migration.ignored {
        let shown = path.strip_prefix(&args.v1_dir).unwrap_or(path);
        println!("Not migrated (Buckle 1.x did not read it either): {}", shown.display());
    }
    let todos = migration.todo_count();
    if todos > 0 {
        println!(
            "{todos} TODO(migrate) comment(s) need a decision. Search for them, then run:\n  buckle validate --config {}",
            args.out_dir.display()
        );
    } else {
        println!("Next: buckle validate --config {}", args.out_dir.display());
    }
    Ok(0)
}
