//! Deployment trait and orchestrator.

pub mod bootstrap;
pub mod clone;
pub mod image;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, NetworkMode};
use crate::nspawn::storage::StorageBackend;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    if let Err(e) = run_deploy_internal(deployer, storage, name, cfg, logs.clone()).await {
        let _ = logs.send(format!("FATAL ERROR: {}", e)).await;
        success.store(false, Ordering::SeqCst);
    } else {
        success.store(true, Ordering::SeqCst);
    }
    done.store(true, Ordering::SeqCst);
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

    // 2. Mount storage (returns rootfs path)
    let rootfs = if !is_ext {
        log::info!(
            "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
            name
        );
        push_log!("Mounting storage...".to_string());
        storage.mount(&name).await?
    } else {
        // For clones, machinectl clone handles everything.
        // We use a dummy path since it won't be used for post-config anyway (skipped below).
        std::path::PathBuf::from(format!("/var/lib/machines/{}", name))
    };

    // Use a scoped guard-like pattern to ensure unmount
    let result = async {
        // 3. Perform base deployment
        log::info!(
            "[AUDIT] [Container: {}] [Step: Deploy] Initiating base rootfs transfer...",
            name
        );
        deployer.deploy(&name, &cfg, &rootfs, logs.clone()).await?;

        // 4. Post-deployment configuration (skipped for clones as they are already configured)
        if is_ext {
            return Ok(());
        }

        if let Some(pwd) = &cfg.root_password {
            push_log!("Setting root password...".to_string());
            crate::nspawn::create::set_root_password(&rootfs, pwd).await?;
        }

        for user in &cfg.users {
            push_log!(format!("Creating user {}...", user.username));
            crate::nspawn::create::create_user_in_container(&rootfs, user).await?;

            if cfg.wayland_socket.is_some() {
                push_log!(format!("Setting up wayland env for {}...", user.username));
                crate::nspawn::create::setup_wayland_shell_env(&rootfs, user).await?;
            }
        }

        let mut nspawn_content = crate::nspawn::create::nspawn_config_content(&cfg);

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());
            if let Ok(state) = crate::nspawn::nvidia::get_nvidia_state().await {
                match crate::nspawn::config::NspawnConfig::apply_gpu_passthrough_to_content(
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

        push_log!("Writing .nspawn config...".to_string());
        let nspawn_path = std::path::PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        if let Some(parent) = nspawn_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }
        std::fs::write(&nspawn_path, nspawn_content)
            .map_err(|e| NspawnError::Io(nspawn_path, e))?;

        if !cfg.device_binds.is_empty() || cfg.nvidia_gpu || cfg.wayland_socket.is_some() {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            crate::nspawn::create::write_systemd_override(
                &name,
                &cfg.device_binds,
                cfg.nvidia_gpu,
                cfg.wayland_socket.is_some(),
            )?;
        }

        if let Some(mode) = &cfg.network {
            if matches!(
                mode,
                NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)
            ) {
                push_log!("Enabling container network (systemd-networkd)...".to_string());
                if let Err(e) = crate::nspawn::create::enable_container_networkd(&rootfs).await {
                    push_log!(format!("WARNING: {} (might not be a systemd container)", e));
                }
            }
        }
        Ok::<(), NspawnError>(())
    }
    .await;

    // 5. Unmount storage
    if !is_ext {
        push_log!("Unmounting storage...".to_string());
        if let Err(e) = storage.unmount(&name).await {
            push_log!(format!("WARNING: Failed to unmount: {}", e));
        }
    }

    if let Err(e) = result {
        return Err(e);
    }

    push_log!("".into());
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}
