//! Deployment trait and orchestrator.

pub mod bootstrap;
pub mod clone;
pub mod image;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, NetworkMode};
use crate::nspawn::storage::StorageBackend;
use crate::nspawn::utils::CommandLogged;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// Orchestrates the asynchronous deployment of a new container.
pub async fn run_deploy_task(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    logs: tokio::sync::mpsc::Sender<String>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
) {
    // 1. Initialize the guard. When this is dropped (at end of function or on panic),
    // it will unconditionally set done = true, unblocking the UI spinner.
    let _guard = DoneGuard { done };

    // 2. Perform deployment
    if let Err(e) = run_deploy_internal(deployer, storage, name.clone(), cfg, logs.clone()).await {
        // Attempt to log the error. We use a non-blocking approach to prevent deadlocks
        // if the log channel happens to be full.
        let err_msg = format!("FATAL ERROR: {}", e);
        match logs.try_send(err_msg.clone()) {
            Ok(_) => {}
            Err(_) => {
                // If channel is full, we log to stdout as fallback
                log::error!("[DEPLOY] [Container: {}] Channel full, cannot send log: {}", name, err_msg);
            }
        }
        success.store(false, Ordering::SeqCst);
    } else {
        success.store(true, Ordering::SeqCst);
    }
}

async fn run_deploy_internal(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    logs: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    macro_rules! push_log {
        ($msg:expr) => {
            let _ = logs.send($msg).await;
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
        storage.create(&name).await?;
    }

    // 2. Deployment & Configuration scoping
    let mut dissect_mount_dir: Option<std::path::PathBuf> = None;

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
            std::path::PathBuf::from(format!("/var/lib/machines/{}", name))
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

        if !tokio::fs::try_exists(&actual_rootfs).await.unwrap_or(false) {
            let raw_path = std::path::PathBuf::from(format!("/var/lib/machines/{}.raw", name));
            if let Ok(meta) = tokio::fs::metadata(&raw_path).await {
                if meta.is_file() {
                    let mount_point =
                        std::path::PathBuf::from(format!("/var/cache/lasper/dissect-{}", name));
                    let _ = tokio::fs::create_dir_all(&mount_point).await;
                    push_log!("Mounting raw image for configuration...".to_string());

                    let out = crate::nspawn::utils::new_command("systemd-dissect")
                        .args([
                            "--mount",
                            raw_path.to_str().unwrap(),
                            mount_point.to_str().unwrap(),
                        ])
                        .logged_output("systemd-dissect")
                        .await;

                    if let Ok(cmd) = out {
                        if cmd.status.success() {
                            actual_rootfs = mount_point.clone();
                            dissect_mount_dir = Some(mount_point);
                        } else {
                            push_log!(
                                "WARNING: Failed to mount raw image with systemd-dissect.".into()
                            );
                        }
                    }
                }
            }
        }

        let is_mounted_dir = if let Ok(meta) = tokio::fs::metadata(&actual_rootfs).await {
            meta.is_dir()
        } else {
            false
        };

        if is_mounted_dir {
            if let Some(pwd) = &cfg.root_password {
                push_log!("Setting root password...".to_string());
                crate::nspawn::rootfs::users::set_root_password(&actual_rootfs, pwd).await?;
            }

            for user in &cfg.users {
                push_log!(format!("Creating user {}...", user.username));
                crate::nspawn::rootfs::users::create_user_in_container(&actual_rootfs, user).await?;

                if cfg.wayland_socket.is_some() {
                    push_log!(format!("Setting up wayland env for {}...", user.username));
                    crate::nspawn::rootfs::wayland::setup_wayland_shell_env(&actual_rootfs, user).await?;
                }
            }
        } else {
            log::warn!("[AUDIT] [Container: {}] rootfs is not a directory. Skipping internal modifications.", name);
            push_log!("WARNING: Target is unmounted. Skipping passwords and user creation.".to_string());
        }

        let xdg_runtime = crate::nspawn::utils::discovery::get_xdg_runtime().await.ok();
        let mut nspawn_content = crate::nspawn::config::nspawn_file::nspawn_config_content(&cfg, xdg_runtime.as_deref())?;

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());
            if let Ok(state) = crate::nspawn::hw::nvidia::get_nvidia_state().await {
                match crate::nspawn::config::nspawn_file::NspawnConfig::apply_gpu_passthrough_to_content(
                    nspawn_content.clone(),
                    &state,
                    &[],
                ) {
                    Ok(mutated) => {
                        nspawn_content = mutated;
                    }
                    Err(e) => {
                        push_log!(format!(
                            "WARNING: Failed to apply NVIDIA AST surgery: {}",
                            e
                        ));
                    }
                }
            }
        }

        // Audit the security posture
        let idmap_supported = crate::nspawn::utils::discovery::supports_idmap();
        if idmap_supported {
            log::info!("[AUDIT] [Container: {}] [Security: High] Using secure idmap for hardware passthrough.", name);
            push_log!("Using secure idmap for hardware passthrough.".to_string());
        } else if cfg.wayland_socket.is_some() || cfg.graphics_acceleration || cfg.privileged {
            log::warn!("[AUDIT] [Container: {}] [Security: Compromised] Disabling User Namespaces due to missing host idmap support.", name);
            push_log!("WARNING: Legacy security mode (PrivateUsers=no) due to missing host idmap support.".to_string());
        }

        if cfg.privileged {
            log::warn!("[AUDIT] [Container: {}] [Security: Dangerous] Privileged mode enabled. Capability=all granted.", name);
            push_log!("DANGER: Privileged mode enabled (Capability=all).".to_string());
        }

        push_log!("Writing .nspawn config...".to_string());
        let nspawn_path = std::path::PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        
        crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&nspawn_path, |_| Ok(nspawn_content)).await?;

        if !cfg.device_binds.is_empty() || cfg.nvidia_gpu || cfg.wayland_socket.is_some() || cfg.graphics_acceleration {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            crate::nspawn::config::systemd_unit::write_systemd_override(
                &name,
                &cfg.device_binds,
                cfg.nvidia_gpu,
                cfg.graphics_acceleration,
                cfg.wayland_socket.is_some(),
            ).await?;
        }

        if is_mounted_dir {
            if let Some(mode) = &cfg.network {
                if matches!(
                    mode,
                    NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)
                ) {
                    push_log!("Enabling container network (systemd-networkd)...".to_string());
                    if let Err(e) = crate::nspawn::rootfs::network::enable_container_networkd(&actual_rootfs).await {
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
        let _ = crate::nspawn::utils::new_command("systemd-dissect")
            .args(["--umount", mnt.to_str().unwrap()])
            .logged_output("systemd-dissect").await;
        let _ = tokio::fs::remove_dir_all(&mnt).await;
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
        
        // Clean up host-side configurations to prevent "ghost configs"
        let nspawn_path = format!("/etc/systemd/nspawn/{}.nspawn", name);
        let override_dir = format!("/etc/systemd/system/systemd-nspawn@{}.service.d", name);
        let _ = tokio::fs::remove_file(&nspawn_path).await;
        let _ = tokio::fs::remove_dir_all(&override_dir).await;

        if is_ext {
            // Cleanup systemd-managed storage (downloaded/imported junk)
            let _ = crate::nspawn::utils::new_command("machinectl")
                .args(["remove", &name])
                .logged_output("machinectl").await;
        } else {
            // Cleanup Lasper-managed storage
            let _ = storage.delete(&name).await;
        }
        return Err(e);
    }

    push_log!("".into());
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}
