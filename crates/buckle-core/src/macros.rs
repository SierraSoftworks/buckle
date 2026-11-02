//! Registration macros. Each expands to an `inventory::submit!` so that a task,
//! fact provider or health check becomes visible to the registries in
//! [`crate::task::registry`], [`crate::facts::registry`] and
//! [`crate::health::registry`] simply by being linked into the binary.

/// Registers a [`crate::task::Task`] implementation (a unit struct).
#[macro_export]
macro_rules! register_task {
    ($t:expr) => {
        $crate::__private::inventory::submit! { $crate::task::TaskRegistration::new(&$t) }
    };
}

/// Registers a [`crate::facts::FactProvider`] implementation (a unit struct).
#[macro_export]
macro_rules! register_facts {
    ($t:expr) => {
        $crate::__private::inventory::submit! { $crate::facts::FactRegistration::new(&$t) }
    };
}

/// Registers a [`crate::health::HealthCheck`] implementation (a unit struct).
#[macro_export]
macro_rules! register_health {
    ($t:expr) => {
        $crate::__private::inventory::submit! { $crate::health::HealthRegistration::new(&$t) }
    };
}
