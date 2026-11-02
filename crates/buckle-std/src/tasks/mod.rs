//! Task kinds. See the design document section 05 for the catalogue.

// Lane 5: files, downloads, commands, users and system settings.
pub(crate) mod hostkit;

pub mod file_absent;
pub mod file_copy;
pub mod file_directory;
pub mod file_symlink;
pub mod file_template;
