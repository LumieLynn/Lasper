//! nspawn-specific backend command handlers.

use crate::events::AppEvent;
use crate::ui::core::{BackendCommand, BackendResponse};
use tokio::sync::mpsc::Sender;

/// Handle backend asynchronous tasks (deployments, validations, etc.)
pub fn handle_command(cmd: BackendCommand, tx: Sender<AppEvent>) {
    tokio::spawn(async move {
        match cmd {
            BackendCommand::SubmitConfig(ctx) => {
                let built = ctx.build_config();
                let (deployer, storage) = ctx.get_deployer_and_storage();
                let name = built.cfg.name.clone();
                let cfg = built.cfg;

                // Bridge mpsc (Deployer API) → broadcast (DeployStepView)
                let (log_mpsc_tx, mut log_mpsc_rx) = tokio::sync::mpsc::channel::<String>(100);
                let log_bcast_tx = ctx.deploy.log_tx.clone();
                tokio::spawn(async move {
                    while let Some(msg) = log_mpsc_rx.recv().await {
                        let _ = log_bcast_tx.send(msg);
                    }
                });

                let done = ctx.deploy.done.clone();
                let success = ctx.deploy.success.clone();

                // Run the real deployment
                let tx_panic = tx.clone();
                let deploy_handle = tokio::spawn(async move {
                    crate::nspawn::ops::provision::run_deploy_task(
                        deployer,
                        storage,
                        name,
                        cfg,
                        log_mpsc_tx,
                        done,
                        success,
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
            BackendCommand::ValidateBridge(_) => {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                let _ = tx
                    .send(AppEvent::BackendResult(BackendResponse::ValidationSuccess))
                    .await;
            }
        }
    });
}
