//! Dispatch process entry points before initializing their runtime or services.

use anyhow::{Context, Result};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use adapters::elevated::{ElevatedDaemon, ElevationMethod};
use cli::{CliDispatch, CliOptions, ShellCommand, ShellIoMode};
use composition::{CompositionMode, DefaultPermissionManager, PermissionLevel, PermissionManager};
use tui::foundation::status::StatusLevel;

mod adapters;
mod application;
mod cli;
mod composition;
mod config;
mod daemon;
mod domain;
mod ipc;
mod logging;
mod paths;
mod tui;

// Shell sessions spend most of their lifetime forwarding terminal I/O. A
// current-thread runtime keeps the idle path to one scheduler thread; PTY
// adapters still obtain blocking workers on demand when a direct attachment
// needs them.
const SHELL_MAX_BLOCKING_THREADS: usize = 4;
const DAEMON_MAX_BLOCKING_THREADS: usize = 8;
const TUI_WORKER_THREADS: usize = 2;
const TUI_MAX_BLOCKING_THREADS: usize = 8;

fn build_current_thread_runtime(max_blocking_threads: usize) -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(max_blocking_threads)
        .build()?)
}

fn build_tui_runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TUI_WORKER_THREADS)
        .max_blocking_threads(TUI_MAX_BLOCKING_THREADS)
        .enable_all()
        .build()?)
}

fn main() -> Result<ExitCode> {
    // 1. Help, version and invalid arguments finish without a runtime,
    //    configuration loading or terminal takeover.
    let dispatch = match cli::dispatch() {
        Ok(dispatch) => dispatch,
        Err(code) => return Ok(ExitCode::from(code as u8)),
    };

    // 2. Only the TUI needs a multithreaded scheduler. Shells and internal
    //    workers obtain blocking threads on demand.
    let runtime = match &dispatch {
        CliDispatch::Application(_) => build_tui_runtime()?,
        CliDispatch::Daemon(_) => build_current_thread_runtime(DAEMON_MAX_BLOCKING_THREADS)?,
        CliDispatch::Shell(_) | CliDispatch::X11Reconcile { .. } => {
            build_current_thread_runtime(SHELL_MAX_BLOCKING_THREADS)?
        }
    };

    // 3. Each entry point owns its configuration, services and cleanup.
    //    The daemon has its own authenticated bootstrap and shutdown path.
    let result = runtime.block_on(async {
        match dispatch {
            CliDispatch::Shell(command) => Ok(run_shell_command(*command).await),
            CliDispatch::Daemon(options) => {
                daemon::daemon_main(
                    options.fd_sock,
                    options.rpc_sock,
                    options.daemon_uid,
                    options.daemon_pid,
                )
                .await
            }
            CliDispatch::X11Reconcile { want_systemd_tools } => {
                run_x11_reconcile(want_systemd_tools).await.map(|()| 0)
            }
            CliDispatch::Application(options) => run_application(options).await.map(|()| 0),
        }
    });

    // 4. Entry-point cleanup has completed. Tokio stdin may still have an
    //    uncancellable blocking read; do not wait for it at process exit.
    runtime.shutdown_background();
    result.map(|code| ExitCode::from(code as u8))
}

async fn start_daemon(
    level: PermissionLevel,
    method: ElevationMethod,
    systemd_tools: bool,
) -> Result<Option<Arc<ElevatedDaemon>>> {
    if level != PermissionLevel::Elevated {
        return Ok(None);
    }
    let daemon = ElevatedDaemon::spawn(method, !systemd_tools)
        .await
        .with_context(|| match method {
            ElevationMethod::Sudo => {
                "failed to start elevated daemon using sudo; for authentication errors, try `sudo -v`"
            }
            ElevationMethod::Run0 => "failed to start elevated daemon using run0",
        })?;
    Ok(Some(Arc::new(daemon)))
}

async fn stop_daemon(daemon: Option<Arc<ElevatedDaemon>>) -> Result<()> {
    if let Some(daemon) = daemon {
        log::info!("Stopping elevated daemon");
        if let Err(error) = daemon.exit().await {
            log::error!("Elevated daemon cleanup failed: {error}");
            return Err(error).context("elevated daemon cleanup failed");
        }
        log::info!("Elevated daemon stopped");
    }
    Ok(())
}

async fn run_x11_reconcile(systemd_tools: bool) -> Result<()> {
    let loaded_config = crate::config::load_config();
    let systemd_tools = systemd_tools || loaded_config.config.settings.systemd_tools;
    let mode =
        crate::composition::CompositionMode::new(crate::composition::PermissionLevel::User, None)?;
    let services = crate::composition::compose_process_shell_services(&mode, systemd_tools);
    let reconcile_result =
        match services.x11_access.reconcile_after_machine_event().await {
            Ok(reports) => {
                let mut incomplete = Vec::new();
                for report in reports {
                    if !report.is_complete() {
                        if !report.pending_record_ids().is_empty() {
                            incomplete.push(format!(
                                "display :{} still has {} pending grant(s)",
                                report.display(),
                                report.pending_record_ids().len()
                            ));
                        }
                        incomplete.extend(report.diagnostics().iter().map(|diagnostic| {
                            format!("display :{}: {diagnostic}", report.display())
                        }));
                    }
                }
                if !incomplete.is_empty() {
                    Err(anyhow::anyhow!(
                        "X11 lifecycle reconcile incomplete: {}",
                        incomplete.join("; ")
                    ))
                } else {
                    Ok(())
                }
            }
            Err(crate::application::x11::X11AccessError::Selection(error)) => Err(anyhow::anyhow!(
                "X11 lifecycle reconcile could not discover a live endpoint: {error}"
            )),
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };
    let activation_result = services.x11_access.synchronize_reconcile_activation().await;
    match (reconcile_result, activation_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(reconcile), Ok(())) => Err(reconcile),
        (Ok(()), Err(activation)) => Err(anyhow::anyhow!(activation.to_string())),
        (Err(reconcile), Err(activation)) => Err(anyhow::anyhow!(
            "{reconcile}; X11 lifecycle activation sync failed: {activation}"
        )),
    }
}

async fn run_shell_command(command: ShellCommand) -> i32 {
    let launch_target = (command.io_mode() == ShellIoMode::Launcher)
        .then(|| format!("{}@{}", command.target().user(), command.target().machine()));
    let loaded_config = crate::config::load_config();
    if let Some(diagnostic) = loaded_config.diagnostic {
        eprintln!("lasper: {}", diagnostic.summary);
    }
    let systemd_tools =
        command.wants_systemd_tools() || loaded_config.config.settings.systemd_tools;
    let use_elevation = command.permits_elevation()
        && DefaultPermissionManager::wants_elevation(
            command.wants_elevation(),
            &loaded_config.config.settings,
        );
    let pm = DefaultPermissionManager::new().with_elevation(use_elevation);
    let elevation_method = command
        .elevation_method()
        .unwrap_or(loaded_config.config.settings.elevation_method);
    let daemon = match start_daemon(pm.level(), elevation_method, systemd_tools).await {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("lasper: {error:#}");
            return 1;
        }
    };
    let mode = match CompositionMode::new(pm.level(), daemon.clone()) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("lasper: failed to compose shell services: {error}");
            if let Err(error) = stop_daemon(daemon).await {
                eprintln!("lasper: {error:#}");
            }
            return 1;
        }
    };
    let services = crate::composition::compose_process_shell_services(&mode, systemd_tools);
    if command.requests_x11() {
        if let Err(error) = services.x11_access.reconcile().await {
            log::debug!("X11 lifecycle reconcile unavailable before shell: {error}");
        }
    }
    let result = if launch_target.is_some() {
        let launch = crate::composition::compose_process_launch_services(
            systemd_tools,
            loaded_config.config.nvidia.source(),
            pm.level(),
        );
        crate::cli::run_launch(
            command,
            &services.session,
            services.x11_access.as_ref(),
            &launch.machine_lifecycle,
            &launch.runtime,
        )
        .await
    } else {
        crate::cli::run_shell(command, &services.session, services.x11_access.as_ref()).await
    };
    drop(services);
    drop(mode);
    let cleanup_failed = match stop_daemon(daemon).await {
        Ok(()) => false,
        Err(error) => {
            eprintln!("lasper: {error:#}");
            true
        }
    };
    let (code, failure) = match result {
        Ok(0) => (0, None),
        Ok(code) => {
            let failure = launch_target.as_ref().map(|_| {
                let failure = format!("The guest command exited with status {code}.");
                eprintln!("lasper: {failure}");
                failure
            });
            (code, failure)
        }
        Err(error) => {
            crate::cli::report_command_error(&error);
            (1, Some(error.to_string()))
        }
    };
    if let (Some(target), Some(failure)) = (launch_target, failure) {
        if let Err(error) =
            crate::adapters::platform::notifications::notify_launch_failure(&target, &failure).await
        {
            eprintln!("lasper: desktop notification unavailable: {error}");
        }
    }
    if code == 0 && cleanup_failed {
        1
    } else {
        code
    }
}

async fn run_application(options: CliOptions) -> Result<()> {
    // 1. Resolve settings once; the same configuration supplies the TUI's
    //    theme and bootstrap profiles later.
    let loaded_config = crate::config::load_config();
    let app_settings = &loaded_config.config.settings;
    let systemd_tools = options.want_systemd_tools || app_settings.systemd_tools;
    let use_elevation =
        DefaultPermissionManager::wants_elevation(options.want_elevation, app_settings);
    let pm: Arc<dyn PermissionManager> =
        Arc::new(DefaultPermissionManager::new().with_elevation(use_elevation));
    let elevation_method = options
        .elevation_method
        .unwrap_or(app_settings.elevation_method);

    // 2. The current user owns the log. Keep its guard through daemon
    //    shutdown, and record startup failures before taking over the terminal.
    let (log_dir, _log_guard) = crate::logging::init()?;
    log::info!("Lasper starting (log dir: {})", log_dir.display());
    log::info!(
        "Process authority: {:?} (launcher: {})",
        pm.level(),
        elevation_method
    );

    // 3. Only privileged operations enter the root daemon. Authenticate
    //    before raw mode so sudo/run0 can use the caller's terminal normally.
    let daemon = start_daemon(pm.level(), elevation_method, systemd_tools)
        .await
        .inspect_err(|error| log::error!("{error:#}"))?;

    // 4. All paths after a successful daemon launch meet at shutdown,
    //    including service composition and terminal initialization failures.
    let result = match CompositionMode::new(pm.level(), daemon.clone()) {
        Ok(mode) => run_tui(loaded_config, pm, mode, systemd_tools).await,
        Err(error) => Err(error.into()),
    };
    if let Err(error) = &result {
        log::error!("Application error: {error:#}");
    }
    let cleanup = stop_daemon(daemon).await;
    log::info!("Lasper stopped");
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("also {cleanup:#}"))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn run_tui(
    loaded_config: crate::config::LoadedConfig,
    permissions: Arc<dyn PermissionManager>,
    mode: CompositionMode,
    systemd_tools: bool,
) -> Result<()> {
    let app_config = Arc::new(loaded_config.config);
    let services = crate::composition::compose_application_services(
        mode,
        systemd_tools,
        app_config.nvidia.source(),
    );
    let recovery_notice = if permissions.level().is_elevated() {
        deployment_recovery_notice(&services.provisioning).await
    } else {
        None
    };
    let mut app = tui::app::App::new(
        permissions,
        systemd_tools,
        app_config.settings.log_buffer_lines,
        services,
        app_config,
    );
    if let Some(diagnostic) = loaded_config.diagnostic {
        log::warn!("{}", diagnostic.detail);
        app.set_status_for(
            diagnostic.summary,
            StatusLevel::Warn,
            Duration::from_secs(12),
        );
    }
    if let Some((message, level)) = recovery_notice {
        app.set_status_for(message, level, Duration::from_secs(20));
    }

    // Display checks stay with shell/access actions, not TUI startup.
    // Terminal takeover and restoration belong to the TUI launcher.
    log::debug!("Starting TUI terminal initialization");
    tui::run(&mut app).await
}

async fn deployment_recovery_notice(
    provisioning: &crate::application::provisioning::ProvisioningService,
) -> Option<(String, StatusLevel)> {
    match provisioning.unfinished_deployments().await {
        Ok(reports) if reports.is_empty() => None,
        Ok(reports) => {
            log::warn!(
                "Found {} unfinished deployment crash manifest(s); automatic replay and rollback are disabled",
                reports.len()
            );
            for report in &reports {
                log::warn!(
                    "Unfinished deployment {} targets {} at revision {} ({:?})",
                    report.manifest.deployment_id,
                    report.manifest.target,
                    report.manifest.revision,
                    report.manifest.state,
                );
                if let Some(error) = &report.probe_error {
                    log::warn!(
                        "Deployment {} host evidence could not be collected: {error}",
                        report.manifest.deployment_id
                    );
                }
                for observation in &report.observations {
                    log::info!(
                        "Deployment {} recovery evidence: {} = {:?} (recorded={:?}, applying={})",
                        report.manifest.deployment_id,
                        observation.subject.resource.label(),
                        observation.evidence,
                        observation.subject.recorded_disposition,
                        observation.subject.applying_when_interrupted,
                    );
                }
            }
            Some((
                format!(
                    "{} previous deployment(s) may be incomplete; inspect before cleanup.",
                    reports.len()
                ),
                StatusLevel::Warn,
            ))
        }
        Err(error) => {
            log::error!("Could not inspect deployment recovery state: {error}");
            Some((
                format!("Could not inspect deployment recovery state: {error}"),
                StatusLevel::Error,
            ))
        }
    }
}
