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
