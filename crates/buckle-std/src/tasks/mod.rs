//! Task kinds. See the design document section 05 for the catalogue.

// Shared helpers.
pub(crate) mod apt_common;
pub(crate) mod hostkit;
pub(crate) mod sys_exec;
pub(crate) mod systemd_common;
pub(crate) mod ufw_common;

// Packages, services and kernel settings.
pub mod apt_package;
pub mod apt_repository;
pub mod apt_update;
pub mod sysctl;
pub mod systemd_daemon_reload;
pub mod systemd_dropin;
pub mod systemd_reload;
pub mod systemd_restart;
pub mod systemd_unit;
pub mod ufw_default;
pub mod ufw_rule;
pub mod ufw_state;

// Files, downloads and archives.
pub mod archive_extract;
pub mod file_absent;
pub mod file_block;
pub mod file_copy;
pub mod file_directory;
pub mod file_download;
pub mod file_line;
pub mod file_symlink;
pub mod file_template;
pub mod file_tree;
pub mod git_repository;
pub mod github_release;

// Commands, users and host settings.
pub mod command_run;
pub mod cron_job;
pub mod docker_image;
pub mod hostname_set;
pub mod ssh_authorized_key;
pub mod swap_file;
pub mod user_account;
pub mod user_group;
