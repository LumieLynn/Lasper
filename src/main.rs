use anyhow::Result;

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

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI flags — all early exits happen here, before terminal
    //    takeover, so raw-mode / alternate-screen restoration is never needed.
    let options = match crate::cli::parse_flags() {
        Ok(options) => options,
        Err(code) => std::process::exit(code),
    };
    let want_elevation = options.want_elevation;
    let mut want_cli_mode = options.want_cli_mode;

    // 1b. Internal daemon mode — run as root child process, exit early.
    if options.is_daemon {
        crate::daemon::daemon_main(
            options.fd_sock,
            options.rpc_sock,
            options.daemon_uid,
            options.daemon_pid,
        )
        .await;
    }

    // 2. Parse configuration once for settings, theme, and bootstrap profiles.
    let loaded_config = crate::config::load_config();
    let config_diagnostic = loaded_config.diagnostic;
    let app_config = std::sync::Arc::new(loaded_config.config);
    let app_settings = &app_config.settings;
    if !want_cli_mode {
        want_cli_mode = app_settings.cli_mode;
    }

    // 3. Permission manager — no full-process elevation.
    //    `-e` / `elevate = true` routes privileged work through a sudo daemon.
    let use_sudo =
        crate::composition::DefaultPermissionManager::wants_elevation(want_elevation, app_settings);
    let pm: std::sync::Arc<dyn crate::composition::PermissionManager> = std::sync::Arc::new(
        crate::composition::DefaultPermissionManager::new().with_elevation(use_sudo),
    );

    // 3b. Spawn elevated daemon before terminal takeover so the sudo
    //     password prompt (if any) appears on the clean terminal.
    let daemon: Option<std::sync::Arc<crate::adapters::elevated::ElevatedDaemon>> =
        if pm.level() == crate::composition::PermissionLevel::Elevated {
            match crate::adapters::elevated::ElevatedDaemon::spawn(!want_cli_mode).await {
                Ok(d) => Some(std::sync::Arc::new(d)),
                Err(e) => {
                    eprintln!("Failed to start elevated daemon: {}", e);
                    eprintln!(
                        "For sudo errors, run `sudo -v`; otherwise inspect the reported \
                         path or permission details."
                    );
                    std::process::exit(1);
                }
            }
        } else {
            None
        };

    // 3c. Validate the authority/daemon pairing before composing host adapters.
    let composition_mode = crate::composition::CompositionMode::new(pm.level(), daemon.clone())?;

    // 4. Setup logging — always owned by the current user.
    let (log_dir, _log_guard) = crate::logging::init()?;

    log::info!("Lasper starting (log dir: {})", log_dir.display());
    log::info!("Elevation mode: {}", use_sudo);

    // 5. Compose and run the application. Terminal ownership stays inside
    // the TUI launcher so a future CLI entry point can bypass it entirely.
    let log_buffer_lines = app_settings.log_buffer_lines;
    let services =
        crate::composition::compose_application_services(composition_mode, want_cli_mode);
    let deployment_recovery = if pm.level() == crate::composition::PermissionLevel::User {
        None
    } else {
        match services.provisioning.unfinished_deployments().await {
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
                    crate::tui::StatusLevel::Warn,
                ))
            }
            Err(error) => {
                log::error!("Could not inspect deployment recovery state: {error}");
                Some((
                    format!("Could not inspect deployment recovery state: {error}"),
                    crate::tui::StatusLevel::Error,
                ))
            }
        }
    };
    let mut app = tui::app::App::new(pm, want_cli_mode, log_buffer_lines, services, app_config);
    if let Some(diagnostic) = config_diagnostic {
        log::warn!("{}", diagnostic.detail);
        app.set_status_for(
            diagnostic.summary,
            crate::tui::StatusLevel::Warn,
            std::time::Duration::from_secs(12),
        );
    }
    if let Some((message, level)) = deployment_recovery {
        app.set_status_for(message, level, std::time::Duration::from_secs(20));
    }
    let result = crate::tui::run(&mut app).await;

    if let Err(ref e) = result {
        log::error!("Application error: {:#}", e);
        eprintln!("Error: {:#}", e);
    }

    log::info!("[lasper] calling daemon.exit()...");
    if let Some(daemon) = daemon {
        daemon.exit().await;
    }
    log::info!("[lasper] daemon.exit() completed");

    log::info!("[lasper] main() returning");
    let code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(code);
}
