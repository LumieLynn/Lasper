//! Deployment trait and orchestrator.

pub(crate) mod bootstrap_operation;
pub mod builders;
pub(crate) mod image_operation;
pub(crate) mod oci_operation;

pub use bootstrap_operation::BootstrapStore;
pub use image_operation::ImageImportStore;
pub use oci_operation::OciPullStore;

use crate::events::AppEvent;
use crate::nspawn::adapters::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, NetworkMode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployProgress {
    pub label: String,
    pub permille: u16,
}

impl DeployProgress {
    pub fn new(label: impl Into<String>, permille: u16) -> Self {
        Self {
            label: label.into(),
            permille: permille.min(1000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployLogEvent {
    Line(String),
    Progress(DeployProgress),
}

/// Side effects that a provider successfully created during deployment.
///
/// A receipt is deliberately returned only after the provider operation has
/// completed successfully.  If an operation fails part-way through, the
/// ownership is unknown and rollback must not destroy a pre-existing image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeploymentReceipt {
    owns_external_image: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageRollback {
    Local,
    ExternalOwned,
    ExternalUnknown,
}

impl DeploymentReceipt {
    pub const fn none() -> Self {
        Self {
            owns_external_image: false,
        }
    }

    pub const fn external_image() -> Self {
        Self {
            owns_external_image: true,
        }
    }

    pub const fn owns_external_image(self) -> bool {
        self.owns_external_image
    }
}

fn storage_rollback(is_external: bool, receipt: DeploymentReceipt) -> StorageRollback {
    if !is_external {
        StorageRollback::Local
    } else if receipt.owns_external_image() {
        StorageRollback::ExternalOwned
    } else {
        StorageRollback::ExternalUnknown
    }
}

/// RAII guard to ensure the 'done' flag is always set, even on panic or early return.
struct DoneGuard {
    done: Arc<AtomicBool>,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
pub trait Deployer: Send + Sync {
    /// Performs the actual deployment (bootstrapping / cloning) of the container.
    async fn deploy(
        &self,
        name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    ) -> Result<DeploymentReceipt>;

    /// Returns true if this deployer manages its own storage (e.g. machinectl clone).
    fn is_external_storage_managed(&self) -> bool {
        false
    }

    /// Returns true if this deployer requires post-deployment configuration (passwords, etc).
    /// Default is true. Clones might set this to false if they are already configured.
    fn requires_post_config(&self) -> bool {
        true
    }
}

pub(crate) async fn send_deploy_log(logs: &Sender<DeployLogEvent>, message: impl Into<String>) {
    let message = message.into();
    log::info!("[DEPLOY] {}", message);
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

pub(crate) async fn send_deploy_stream_log(
    logs: &Sender<DeployLogEvent>,
    message: impl Into<String>,
) {
    let message = message.into();
    if is_high_signal_deploy_stream(&message) {
        log::warn!("[DEPLOY stream] {}", message);
    } else {
        // Deployment output must survive the wizard and be available in the
        // normal per-run log without requiring RUST_LOG=debug.
        log::info!("[DEPLOY stream] {}", message);
    }
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

fn is_high_signal_deploy_stream(message: &str) -> bool {
    let message = message.trim_start().to_ascii_lowercase();
    ["w:", "e:", "warning:", "error:", "fatal:"]
        .iter()
        .any(|prefix| message.starts_with(prefix))
        || [
            "permission denied",
            "operation not permitted",
            "failed",
            "failure",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

pub(crate) async fn send_deploy_progress(
    logs: &Sender<DeployLogEvent>,
    label: impl Into<String>,
    permille: u16,
) {
    let progress = DeployProgress::new(label, permille);
    log::trace!(
        "[DEPLOY progress] {}: {}.{:01}%",
        progress.label,
        progress.permille / 10,
        progress.permille % 10
    );
    let _ = logs.send(DeployLogEvent::Progress(progress)).await;
}

/// Orchestrates the asynchronous deployment of a new container.
#[allow(clippy::too_many_arguments)]
pub async fn run_deploy_task(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
    exec_ctx: std::sync::Arc<crate::nspawn::sys::ExecutionContext>,
    logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
) {
    // 1. Initialize the guard. When this is dropped (at end of function or on panic),
    // it will unconditionally set done = true, unblocking the UI spinner.
    let _guard = DoneGuard { done };

    // 2. Perform deployment
    if let Err(e) = run_deploy_internal(
        deployer,
        storage,
        name.clone(),
        cfg,
        nvidia_profile,
        exec_ctx,
        logs.clone(),
    )
    .await
    {
        // Attempt to log the error. We use a non-blocking approach to prevent deadlocks
        // if the log channel happens to be full.
        let err_msg = format!("FATAL ERROR: {}", e);
        match logs.try_send(DeployLogEvent::Line(err_msg.clone())) {
            Ok(_) => {}
            Err(_) => {
                // If channel is full, we log to stdout as fallback
                log::error!(
                    "[DEPLOY] [Container: {}] Channel full, cannot send log: {}",
                    name,
                    err_msg
                );
            }
        }
        success.store(false, Ordering::SeqCst);
        let _ = tx
            .send(AppEvent::BackendResult(
                crate::nspawn::ops::BackendResponse::DeployFailed(e.to_string()),
            ))
            .await;
    } else {
        success.store(true, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_deploy_internal(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
    exec_ctx: std::sync::Arc<crate::nspawn::sys::ExecutionContext>,
    logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
) -> Result<()> {
    crate::nspawn::models::NspawnConfigSpec::try_from(&cfg)?;
    let system_operations = exec_ctx.system_operations.clone();

    macro_rules! push_log {
        ($msg:expr) => {
            send_deploy_log(&logs, $msg).await;
        };
    }

    push_log!(format!("=== Deploying '{}' ===", name));

    // 1. Create storage
    let is_ext = deployer.is_external_storage_managed();
    let mut receipt = DeploymentReceipt::none();
    if !is_ext {
        log::info!(
            "[AUDIT] [Container: {}] [Step: Storage] Creating {} storage...",
            name,
            storage.get_type().label()
        );
        push_log!(format!(
            "Creating storage (type: {:?})...",
            storage.get_type()
        ));
        storage.create(&name).await?;
    }

    // 2. Deployment & Configuration scoping
    let mut raw_mount_target: Option<crate::nspawn::adapters::rootfs::RootfsTarget> = None;

    let result = async {
        // 2. Mount storage (returns rootfs path)
        let rootfs = if !is_ext {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
                name
            );
            push_log!("Mounting storage...".to_string());
            storage.mount(&name).await?
        } else {
            // For externally managed storage (clone/pull), the machine is already in /var/lib/machines.
            crate::paths::machine_root(&name)
        };

        // 3. Perform base deployment
        log::info!(
            "[AUDIT] [Container: {}] [Step: Deploy] Initiating base rootfs transfer...",
            name
        );
        receipt = deployer.deploy(&name, &cfg, &rootfs, logs.clone()).await?;

        // 4. Post-deployment configuration
        if !deployer.requires_post_config() {
            log::info!("[AUDIT] [Container: {}] [Step: Config] Skipping post-config for pre-configured clones.", name);
            return Ok(());
        }

        let mut actual_rootfs_target =
            crate::nspawn::adapters::rootfs::RootfsTarget::from_provisioned_path(&name, &rootfs)?;
        let rootfs_exists = exec_ctx
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        if !rootfs_exists && actual_rootfs_target.supports_raw_fallback() {
            push_log!("Mounting raw image for configuration...".to_string());
            match exec_ctx.rootfs.mount_managed_raw(&name).await {
                Ok(Some(target)) => {
                    actual_rootfs_target = target.clone();
                    raw_mount_target = Some(target);
                }
                Ok(None) => {}
                Err(error) => {
                    push_log!(format!(
                        "WARNING: Failed to mount raw image with systemd-dissect: {}",
                        error
                    ));
                }
            }
        }

        let has_os_layout = exec_ctx
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        let supports_offline_commands = has_os_layout
            && exec_ctx
                .rootfs
                .supports_nspawn_commands(&actual_rootfs_target)
                .await?;

        if supports_offline_commands {
            if let Some(pwd) = &cfg.root_password {
                push_log!("Setting root password...".to_string());
                for warning in exec_ctx
                    .rootfs
                    .set_root_password(&actual_rootfs_target, pwd)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
            }

            for user in &cfg.users {
                push_log!(format!("Creating user {}...", user.username));
                for warning in exec_ctx
                    .rootfs
                    .create_user(&actual_rootfs_target, user)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }

                if cfg.wayland_socket.is_some() {
                    push_log!(format!("Setting up wayland env for {}...", user.username));
                    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
                    exec_ctx
                        .rootfs
                        .configure_wayland(&actual_rootfs_target, user, &display)
                        .await?;
                }
            }
        } else if !has_os_layout {
            log::warn!("[AUDIT] [Container: {}] rootfs OS layout could not be verified. Skipping internal modifications.", name);
            push_log!("WARNING: Could not verify the rootfs OS layout. Skipping passwords and user creation.".to_string());
        } else if cfg.root_password.is_some() || !cfg.users.is_empty() {
            log::warn!("[AUDIT] [Container: {}] rootfs has no /usr tree required by systemd-nspawn offline commands. Skipping account modifications.", name);
            push_log!("WARNING: This rootfs has no /usr tree required by systemd-nspawn; skipping password and user creation.".to_string());
        }

        let xdg_runtime = crate::nspawn::platform::capabilities::get_xdg_runtime()
            .await
            .ok();
        let mut initial_nvidia_state = None;

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());

            // Save profile so lifecycle.rs can reload it on container start
            if let Some(prof) = &nvidia_profile {
                let _ = prof.save(&name, &exec_ctx.nvidia_state).await;
            }

            // Run initial CDI discovery to seed the .nspawn config and state.
            // Remapping is applied inside get_nvidia_state after CDI + ldconfig collection.
            if let Ok(state) = crate::nspawn::platform::nvidia::get_nvidia_state(
                nvidia_profile.as_ref(),
            )
            .await
            {
                // Persist initial state for lifecycle diffing
                if let Err(e) = exec_ctx.nvidia_state.write(&name, &state).await {
                    push_log!(format!("WARNING: Failed to save NVIDIA state: {}", e));
                }

                // Write ld.so.conf.d and env vars into rootfs (one-time setup)
                if supports_offline_commands {
                    match crate::nspawn::platform::nvidia::lifecycle::inject_env_once(
                        &actual_rootfs_target,
                        &state,
                        &exec_ctx.rootfs,
                    )
                    .await
                    {
                        Ok(warnings) => {
                            for warning in warnings {
                                log::warn!("{}", warning);
                                push_log!(warning);
                            }
                        }
                        Err(error) => {
                            push_log!(format!(
                                "WARNING: Failed to inject NVIDIA env/ldconfig: {}",
                                error
                            ));
                        }
                    }
                } else if has_os_layout {
                    push_log!("WARNING: Skipping NVIDIA env/ldconfig injection because this rootfs cannot run systemd-nspawn offline commands.".to_string());
                } else {
                    push_log!("WARNING: Skipping NVIDIA env/ldconfig injection because the rootfs OS layout could not be verified.".to_string());
                }
                initial_nvidia_state = Some(state);
            } else {
                push_log!("WARNING: NVIDIA CDI discovery failed. GPU passthrough will be retried on container start.".to_string());
            }
        }

        if cfg.private_users == Some(crate::nspawn::models::PrivateUsersMode::No) {
            log::warn!("[AUDIT] [Container: {}] [Security] PrivateUsers=no, user namespacing disabled.", name);
            push_log!("WARNING: PrivateUsers=no, user namespacing disabled.".to_string());
        }

        if cfg.privileged {
            log::warn!("[AUDIT] [Container: {}] [Security: Dangerous] Privileged mode enabled. Capability=all granted.", name);
            push_log!("DANGER: Privileged mode enabled (Capability=all).".to_string());
        }

        push_log!("Writing .nspawn config...".to_string());
        exec_ctx
            .nspawn
            .write_generated(
                &cfg,
                xdg_runtime.as_deref(),
                initial_nvidia_state.as_ref(),
            )
            .await?;

        if !cfg.device_binds.is_empty() || cfg.nvidia_gpu || cfg.wayland_socket.is_some() || cfg.graphics_acceleration {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            exec_ctx.systemd_unit.write_override(
                &name,
                &cfg.device_binds,
                cfg.nvidia_gpu,
                cfg.graphics_acceleration,
                cfg.wayland_socket.is_some(),
            ).await?;

            let _ = system_operations.reload_daemon().await;
        }

        if supports_offline_commands {
            if let Some(mode) = &cfg.network {
                if matches!(
                    mode,
                    NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)
                ) {
                    push_log!("Enabling container network (systemd-networkd)...".to_string());
                    if let Err(e) = exec_ctx.rootfs.configure_network(&actual_rootfs_target).await {
                        push_log!(format!("WARNING: {} (might not be a systemd container)", e));
                    }
                }
            }
        }
        Ok::<(), NspawnError>(())
    }
    .await;

    // ---- Cleanup Guard ----

    // 1. Unmount the managed raw-image configuration mount if it was used
    if let Some(target) = raw_mount_target {
        push_log!("Unmounting raw image...".to_string());
        if let Err(error) = exec_ctx.rootfs.unmount_managed_raw(&target).await {
            log::warn!(
                "Failed to clean up raw image configuration mount: {}",
                error
            );
        }
    }

    // 2. Unmount Lasper storage
    if !is_ext {
        push_log!("Unmounting storage...".to_string());
        let _ = storage.unmount(&name).await;
    }

    // 3. Transactional Rollback
    if let Err(e) = result {
        push_log!(format!("Deployment failed: {}", e));
        push_log!("Rolling back broken container...".to_string());

        match storage_rollback(is_ext, receipt) {
            StorageRollback::ExternalOwned => {
                // Cleanup only systemd-managed storage that this deployment
                // successfully created and explicitly acknowledged.
                let _ = exec_ctx.nspawn.remove(&name).await;
                let _ = exec_ctx.systemd_unit.remove_overrides(&name).await;
                let _ = exec_ctx.system_operations.remove_image(&name).await;
            }
            StorageRollback::ExternalUnknown => {
                // The provider failed before ownership was established.  Do
                // not call either image removal or local storage deletion.
                log::warn!(
                    "External deployment for {} failed before ownership was confirmed; leaving storage untouched",
                    name
                );
            }
            StorageRollback::Local => {
                // Local storage was created by this deployment, so its
                // generated host configuration is owned by the same attempt.
                let _ = exec_ctx.nspawn.remove(&name).await;
                let _ = exec_ctx.systemd_unit.remove_overrides(&name).await;
                let _ = storage.delete(&name).await;
            }
        }
        return Err(e);
    }

    push_log!("");
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_high_signal_deploy_stream, storage_rollback, DeploymentReceipt, StorageRollback,
    };

    #[test]
    fn deploy_stream_classifies_recoverable_bootstrap_diagnostics() {
        for message in [
            "W: Failure trying to run: test-dev-null",
            "test-dev-null: Permission denied",
            "E: bootstrap failed",
            "operation not permitted while probing target",
        ] {
            assert!(
                is_high_signal_deploy_stream(message),
                "did not classify {message:?}"
            );
        }
    }

    #[test]
    fn deploy_stream_distinguishes_normal_output_from_warnings() {
        for message in [
            "I: Retrieving base-files",
            "I: Extracting base-passwd",
            "Download complete",
        ] {
            assert!(
                !is_high_signal_deploy_stream(message),
                "misclassified {message:?}"
            );
        }
    }

    #[test]
    fn external_rollback_is_allowed_only_with_an_ownership_receipt() {
        assert_eq!(
            storage_rollback(true, DeploymentReceipt::none()),
            StorageRollback::ExternalUnknown
        );
        assert_eq!(
            storage_rollback(true, DeploymentReceipt::external_image()),
            StorageRollback::ExternalOwned
        );
    }

    #[test]
    fn external_failure_never_falls_back_to_local_storage_cleanup() {
        assert_eq!(
            storage_rollback(true, DeploymentReceipt::none()),
            StorageRollback::ExternalUnknown
        );
        assert_eq!(
            storage_rollback(false, DeploymentReceipt::none()),
            StorageRollback::Local
        );
    }
}
