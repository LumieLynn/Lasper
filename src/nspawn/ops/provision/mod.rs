//! Deployment trait and orchestrator.

pub mod backend;
pub mod builders;

use crate::events::AppEvent;
use crate::nspawn::adapters::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, NetworkMode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

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
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()>;

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

pub(crate) async fn send_deploy_log(logs: &Sender<String>, message: impl Into<String>) {
    let message = message.into();
    log::info!("[DEPLOY] {}", message);
    let _ = logs.send(message).await;
}

pub(crate) async fn send_deploy_stream_log(logs: &Sender<String>, message: impl Into<String>) {
    let message = message.into();
    log::debug!("[DEPLOY stream] {}", message);
    let _ = logs.send(message).await;
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
    logs: tokio::sync::mpsc::Sender<String>,
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
        match logs.try_send(err_msg.clone()) {
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
    logs: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    let io = exec_ctx.io.clone();
    let cli_runner = exec_ctx.cmd.clone();
    let provision: std::sync::Arc<dyn crate::nspawn::ops::provision::backend::ProvisionBackend> =
        std::sync::Arc::new(crate::nspawn::adapters::comm::cli::CliBackend::new(
            cli_runner.clone(),
        ));

    macro_rules! push_log {
        ($msg:expr) => {
            send_deploy_log(&logs, $msg).await;
        };
    }

    push_log!(format!("=== Deploying '{}' ===", name));

    // 1. Create storage
    let is_ext = deployer.is_external_storage_managed();
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
        storage.create(&name, cli_runner.as_ref(), &io).await?;
    }

    // 2. Deployment & Configuration scoping
    let mut dissect_mount_dir: Option<std::path::PathBuf> = None;
    let mut _dissect_guard: Option<tempfile::TempDir> = None;

    let result = async {
        // 2. Mount storage (returns rootfs path)
        let rootfs = if !is_ext {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
                name
            );
            push_log!("Mounting storage...".to_string());
            storage.mount(&name, cli_runner.as_ref(), &io).await?
        } else {
            // For externally managed storage (clone/pull), the machine is already in /var/lib/machines.
            crate::paths::machine_root(&name)
        };

        // 3. Perform base deployment
        log::info!(
            "[AUDIT] [Container: {}] [Step: Deploy] Initiating base rootfs transfer...",
            name
        );
        deployer.deploy(&name, &cfg, &rootfs, logs.clone()).await?;

        // 4. Post-deployment configuration
        if !deployer.requires_post_config() {
            log::info!("[AUDIT] [Container: {}] [Step: Config] Skipping post-config for pre-configured clones.", name);
            return Ok(());
        }

        // ---- systemd-dissect raw mounting ----
        let mut actual_rootfs = rootfs.clone();

        // Check via ElevatedIo so Elevated mode can verify the rootfs
        let rootfs_exists = io
            .read_to_string(&actual_rootfs.join("etc/os-release"))
            .await?
            .is_some();
        if !rootfs_exists {
            let raw_path = crate::paths::machine_raw_image(&name);
            let raw_exists = io
                .read_to_string(&raw_path)
                .await
                .ok()
                .flatten()
                .is_some();
            if raw_exists {
                let dissect_parent = "/var/cache/lasper/mounts";
                let _ = io.create_dir_all(std::path::Path::new(dissect_parent)).await;

                let tmp_mnt = tempfile::Builder::new()
                    .prefix(&format!("lasper-dissect-{}-", name))
                    .tempdir_in(dissect_parent)
                    .map_err(|e| NspawnError::Runtime(format!("Failed to create temporary mount point: {}", e)))?;

                let mount_point = tmp_mnt.path().to_path_buf();
                push_log!("Mounting raw image for configuration...".to_string());

                let out = cli_runner
                    .run(
                        "systemd-dissect",
                        vec![
                            "--mount".into(),
                            raw_path.to_str().unwrap().into(),
                            mount_point.to_str().unwrap().into(),
                        ],
                    )
                    .await;
                if let Ok(ref cmd) = out {
                    crate::nspawn::sys::log_output("systemd-dissect", cmd);
                }

                if let Ok(cmd) = out {
                    if cmd.status.success() {
                        actual_rootfs = mount_point.clone();
                        dissect_mount_dir = Some(mount_point);
                        _dissect_guard = Some(tmp_mnt);
                    } else {
                        push_log!("WARNING: Failed to mount raw image with systemd-dissect.");
                    }
                }
            }
        }

        let is_mounted_dir = io
            .read_to_string(&actual_rootfs.join("etc/os-release"))
            .await?
            .is_some();

        if is_mounted_dir {
            if let Some(pwd) = &cfg.root_password {
                push_log!("Setting root password...".to_string());
                crate::nspawn::adapters::rootfs::users::set_root_password(&actual_rootfs, pwd, &logs, cli_runner.as_ref()).await?;
            }

            for user in &cfg.users {
                push_log!(format!("Creating user {}...", user.username));
                crate::nspawn::adapters::rootfs::users::create_user_in_container(&actual_rootfs, user, &logs, cli_runner.as_ref(), &io).await?;

                if cfg.wayland_socket.is_some() {
                    push_log!(format!("Setting up wayland env for {}...", user.username));
                    crate::nspawn::adapters::rootfs::wayland::setup_wayland_shell_env(&actual_rootfs, user, &io).await?;
                }
            }
        } else {
            log::warn!("[AUDIT] [Container: {}] rootfs is not a directory. Skipping internal modifications.", name);
            push_log!("WARNING: Target is unmounted. Skipping passwords and user creation.".to_string());
        }

        let xdg_runtime = crate::nspawn::platform::capabilities::get_xdg_runtime()
            .await
            .ok();
        let mut initial_nvidia_state = None;

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());

            // Save profile so lifecycle.rs can reload it on container start
            if let Some(prof) = &nvidia_profile {
                let _ = prof.save(&name, &io).await;
            }

            // Run initial CDI discovery to seed the .nspawn config and state.
            // Remapping is applied inside get_nvidia_state after CDI + ldconfig collection.
            if let Ok(state) = crate::nspawn::platform::nvidia::get_nvidia_state(
                nvidia_profile.as_ref(),
            )
            .await
            {
                // Persist initial state for lifecycle diffing
                if let Err(e) = crate::nspawn::platform::nvidia::state::save_external_state(
                    &name, &state, &io,
                )
                .await
                {
                    push_log!(format!("WARNING: Failed to save NVIDIA state: {}", e));
                }

                // Write ld.so.conf.d and env vars into rootfs (one-time setup)
                if let Err(e) = crate::nspawn::platform::nvidia::lifecycle::inject_env_once(
                    &name, &state, &io, cli_runner.as_ref(),
                )
                .await
                {
                    push_log!(format!("WARNING: Failed to inject NVIDIA env/ldconfig: {}", e));
                }
                initial_nvidia_state = Some(state);
            } else {
                push_log!("WARNING: NVIDIA CDI discovery failed. GPU passthrough will be retried on container start.".to_string());
            }
        }

        if cfg.private_users.as_deref() == Some("no") {
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
            crate::nspawn::adapters::config::systemd_unit::write_systemd_override(
                &name,
                &cfg.device_binds,
                cfg.nvidia_gpu,
                cfg.graphics_acceleration,
                cfg.wayland_socket.is_some(),
                &io,
            ).await?;

            let _ = provision.reload_daemon().await;
        }

        if is_mounted_dir {
            if let Some(mode) = &cfg.network {
                if matches!(
                    mode,
                    NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)
                ) {
                    push_log!("Enabling container network (systemd-networkd)...".to_string());
                    if let Err(e) = crate::nspawn::adapters::rootfs::network::enable_container_networkd(&actual_rootfs, cli_runner.as_ref(), &io).await {
                        push_log!(format!("WARNING: {} (might not be a systemd container)", e));
                    }
                }
            }
        }
        Ok::<(), NspawnError>(())
    }
    .await;

    // ---- Cleanup Guard ----

    // 1. Unmount systemd-dissect if it was used
    if let Some(mnt) = dissect_mount_dir {
        push_log!("Unmounting raw image...".to_string());
        let _ = cli_runner
            .run(
                "systemd-dissect",
                vec!["--umount".into(), mnt.to_str().unwrap().into()],
            )
            .await;
        // _dissect_guard will automatically clean up the directory when it drops
    }

    // 2. Unmount Lasper storage
    if !is_ext {
        push_log!("Unmounting storage...".to_string());
        let _ = storage.unmount(&name, cli_runner.as_ref(), &io).await;
    }

    // 3. Transactional Rollback
    if let Err(e) = result {
        push_log!(format!("Deployment failed: {}", e));
        push_log!("Rolling back broken container...".to_string());

        // Clean up host-side configurations to prevent "ghost configs"
        let override_dir = format!("/etc/systemd/system/systemd-nspawn@{}.service.d", name);
        let _ = exec_ctx.nspawn.remove(&name).await;
        let _ = io.remove_dir_all(std::path::Path::new(&override_dir)).await;

        if is_ext {
            // Cleanup systemd-managed storage (downloaded/imported junk)
            let _ = cli_runner
                .run("machinectl", vec!["remove".into(), name.clone()])
                .await;
        } else {
            // Cleanup Lasper-managed storage
            let _ = storage.delete(&name, cli_runner.as_ref(), &io).await;
        }
        return Err(e);
    }

    push_log!("");
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}
