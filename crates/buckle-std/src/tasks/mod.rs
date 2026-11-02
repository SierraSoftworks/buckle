//! Task kinds. See the design document section 05 for the catalogue.

// Lane 6: apt, systemd, sysctl and ufw kinds.
pub(crate) mod apt_common;
pub mod apt_package;
pub mod apt_repository;
pub mod apt_update;
pub(crate) mod sys_exec;
pub mod sysctl;
pub(crate) mod systemd_common;
pub mod systemd_daemon_reload;
pub mod systemd_dropin;
pub mod systemd_reload;
pub mod systemd_restart;
pub mod systemd_unit;
pub(crate) mod ufw_common;
pub mod ufw_default;
pub mod ufw_rule;
pub mod ufw_state;
