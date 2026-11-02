//! Task kinds. See the design document section 05 for the catalogue.

// Lane 6: apt, systemd, sysctl and ufw kinds.
pub(crate) mod apt_common;
pub mod apt_package;
pub mod apt_repository;
pub mod apt_update;
pub(crate) mod sys_exec;
