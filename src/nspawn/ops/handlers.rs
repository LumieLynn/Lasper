//! nspawn-specific backend command handlers.

use super::{BackendCommand, BackendResponse};
use crate::events::AppEvent;
use tokio::sync::mpsc::Sender;

/// Handle backend asynchronous tasks (deployments, validations, etc.)
pub fn handle_command(cmd: BackendCommand, tx: Sender<AppEvent>) {
    let tx_panic = tx.clone();
    tokio::spawn(async move {
        let result = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            dispatch_command(cmd, tx),
        ))
        .await;

        if let Err(panic_err) = result {
            let msg = format!(
                "Backend task panicked: {}",
                panic_err
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("(unknown)")
            );
            log::error!("{}", msg);
            let _ = tx_panic
                .send(AppEvent::ActionDone(msg, crate::ui::StatusLevel::Error))
                .await;
        }
    });
}

async fn dispatch_command(cmd: BackendCommand, tx: Sender<AppEvent>) {
    match cmd {
        BackendCommand::SubmitConfig(ctx) => {
            let exec_ctx = ctx.exec_ctx.clone();
            let system_operations = exec_ctx.system_operations.clone();
            let nspawn = exec_ctx.nspawn.clone();
            let systemd_unit = exec_ctx.systemd_unit.clone();
            let managed_storage = exec_ctx.managed_storage.clone();
            let bootstrap = exec_ctx.bootstrap.clone();
            let image_import = exec_ctx.image_import.clone();
            let oci_pull = exec_ctx.oci_pull.clone();

            if ctx.source.remote_tar_url().is_some() && !ctx.source.unsafe_remote_tar_accepted() {
                match image_import.assess_tar_runtime().await {
                    Ok(assessment) => {
                        if let Some(risk) = assessment.risk {
                            let _ = tx
                                .send(AppEvent::BackendResult(
                                    BackendResponse::TarImportRiskConfirmationRequired(risk),
                                ))
                                .await;
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx
                            .send(AppEvent::BackendResult(BackendResponse::ValidationError(
                                format!("Could not inspect the host tar runtime: {error}"),
                            )))
                            .await;
                        return;
                    }
                }
            }

            let built = ctx.build_config();
            let (deployer, storage) = ctx.get_deployer_and_storage(
                system_operations,
                nspawn,
                systemd_unit,
                managed_storage,
                bootstrap,
                image_import,
                oci_pull,
            );
            let name = built.cfg.name.clone();
            let cfg = built.cfg;
            let nvidia_profile = built.nvidia_profile;

            // Bridge mpsc (Deployer API) → broadcast (DeployStepView)
            let (log_mpsc_tx, mut log_mpsc_rx) =
                tokio::sync::mpsc::channel::<crate::nspawn::ops::provision::DeployLogEvent>(100);
            let log_bcast_tx = ctx.deploy.log_tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = log_mpsc_rx.recv().await {
                    let _ = log_bcast_tx.send(msg);
                }
            });

            let done = ctx.deploy.done.clone();
            let success = ctx.deploy.success.clone();
            let cancelled = ctx.deploy.cancelled.clone();
            let rolling_back = ctx.deploy.rolling_back.clone();
            let cancellation = ctx.deploy.cancellation.clone();

            // Run the real deployment
            let tx_panic = tx.clone();
            let tx_deploy = tx.clone();
            let operation = exec_ctx.host_operations.begin();
            let deploy_handle = tokio::spawn(async move {
                let _operation = operation;
                crate::nspawn::ops::provision::run_deploy_task(
                    deployer,
                    storage,
                    name,
                    cfg,
                    nvidia_profile,
                    exec_ctx,
                    log_mpsc_tx,
                    done,
                    success,
                    cancelled,
                    rolling_back,
                    cancellation,
                    tx_deploy,
                )
                .await;
            });

            // Monitor for panics
            tokio::spawn(async move {
                if let Err(join_err) = deploy_handle.await {
                    if join_err.is_panic() {
                        let _ = tx_panic
                            .send(AppEvent::ActionDone(
                                "CRITICAL: Deployment pipeline panicked.".into(),
                                crate::ui::StatusLevel::Error,
                            ))
                            .await;
                    }
                }
            });

            let _ = tx
                .send(AppEvent::BackendResult(BackendResponse::DeployStarted))
                .await;
        }
        BackendCommand::ValidateInterface {
            name,
            is_bridge_mode,
        } => {
            let net_path = format!("/sys/class/net/{}", name);
            let bridge_path = format!("/sys/class/net/{}/bridge", name);

            let exists = tokio::fs::metadata(&net_path).await.is_ok();
            let is_bridge = tokio::fs::metadata(&bridge_path).await.is_ok();

            let resp = if !exists {
                BackendResponse::ValidationWarning(format!(
                    "Interface '{}' not found. It must exist before starting the container.",
                    name
                ))
            } else if is_bridge_mode && !is_bridge {
                let actual_type = crate::nspawn::platform::network::identify_interface(&name).await;
                BackendResponse::ValidationWarning(format!(
                    "'{}' is a {}, not a bridge",
                    name, actual_type
                ))
            } else if !is_bridge_mode && is_bridge {
                BackendResponse::ValidationWarning(format!(
                    "'{}' is a bridge, but you selected a physical/virtual mode",
                    name
                ))
            } else {
                BackendResponse::ValidationSuccess
            };

            let _ = tx.send(AppEvent::BackendResult(resp)).await;
        }
        BackendCommand::DiscoverHardware => {
            let _ = tx
                .send(AppEvent::BackendResult(BackendResponse::DiscoveryStarted))
                .await;

            let discovery_res =
                crate::nspawn::platform::nvidia::discovery::discover_hardware().await;
            let gpus = crate::nspawn::platform::gpu::discover_host_gpus().await;

            match discovery_res {
                Ok((nvidia_devices, nvidia_state)) => {
                    let _ = tx
                        .send(AppEvent::BackendResult(
                            BackendResponse::HardwareDiscovered {
                                nvidia_state,
                                nvidia_devices,
                                host_gpus: gpus,
                            },
                        ))
                        .await;
                }
                Err(e) => {
                    log::error!("Hardware discovery failed: {}", e);
                    let _ = tx
                        .send(AppEvent::BackendResult(BackendResponse::DiscoveryFailed(
                            e.to_string(),
                        )))
                        .await;
                }
            }
        }
    }
}
