//! The Windows Service Control Manager entry point for `buckle agent`
//! (`--windows-service`, which `buckle up` puts in the service's
//! command line). The SCM calls `service_main` on its own thread; it reports
//! the service as running, runs the agent on the process's tokio runtime and
//! turns Stop and Shutdown requests into a graceful shutdown (the agent
//! finishes a running apply first).

use buckle_agent::AgentConfig;
use std::ffi::OsString;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// What `service_main` needs from `run`, which the SCM cannot pass through.
struct Launch {
    runtime: tokio::runtime::Handle,
    config: Mutex<Option<AgentConfig>>,
    exit: Mutex<Option<buckle_core::Result<i32>>>,
}

static LAUNCH: OnceLock<Launch> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the SCM dispatcher and returns the agent's exit
/// code once the service has stopped.
pub async fn run(config: AgentConfig) -> buckle_core::Result<i32> {
    let launch = Launch {
        runtime: tokio::runtime::Handle::current(),
        config: Mutex::new(Some(config)),
        exit: Mutex::new(None),
    };
    if LAUNCH.set(launch).is_err() {
        return Err(buckle_core::error::system(
            "The Windows service was started twice in one process.",
            &["This is a bug; please report it."],
        ));
    }
    tokio::task::spawn_blocking(|| {
        service_dispatcher::start(buckle_agent::install::SERVICE_NAME, ffi_service_main)
    })
    .await
    .map_err(|e| {
        buckle_core::error::wrap_system(e, "The service dispatcher thread panicked.", &[])
    })?
    .map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            "Could not connect to the Windows Service Control Manager.",
            &["`--windows-service` is for the service's own command line; run `buckle agent --config <path>` interactively instead."],
        )
    })?;
    LAUNCH
        .get()
        .and_then(|l| l.exit.lock().ok().and_then(|mut e| e.take()))
        .unwrap_or(Ok(0))
}

fn status(state: ServiceState, exit: u32, wait_hint: Duration) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(exit),
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let Some(launch) = LAUNCH.get() else { return };
    let Some(config) = launch.config.lock().ok().and_then(|mut c| c.take()) else {
        return;
    };
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let handle = match service_control_handler::register(
        buckle_agent::install::SERVICE_NAME,
        handler,
    ) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(error = %err, "could not register with the Service Control Manager");
            return;
        }
    };
    let _ = handle.set_service_status(status(ServiceState::Running, 0, Duration::ZERO));
    let result = launch.runtime.block_on(buckle_agent::run_with_version(
        config,
        crate::telemetry::version(),
        shutdown,
    ));
    let code = match &result {
        Ok(code) => *code as u32,
        Err(err) => {
            tracing::error!(error = %err.description(), "the agent stopped with an error");
            1
        }
    };
    if let Ok(mut exit) = launch.exit.lock() {
        *exit = Some(result);
    }
    let _ = handle.set_service_status(status(ServiceState::Stopped, code, Duration::ZERO));
}
