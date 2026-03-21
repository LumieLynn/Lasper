//! Deployment trait and orchestrator.

pub mod bootstrap;
pub mod image;
pub mod clone;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};

#[async_trait::async_trait]
pub trait Deployer: Send + Sync {
    /// Performs the actual deployment (bootstrapping / cloning) of the container.
    async fn deploy(
        &self,
        name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: Arc<Mutex<Vec<String>>>,
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
    logs: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
) {
    if let Err(e) = run_deploy_internal(deployer, storage, name, cfg, logs.clone()).await {
        let mut l = logs.lock().unwrap();
        l.push(format!("FATAL ERROR: {}", e));
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
    logs: Arc<Mutex<Vec<String>>>,
) -> Result<()> {
    let push_log = |s: String| {
        let mut l = logs.lock().unwrap();
        l.push(s);
    };

    push_log(format!("=== Deploying '{}' ===", name));

    // 1. Create storage
    let is_ext = deployer.is_external_storage_managed();
    if !is_ext {
        push_log(format!("Creating storage (type: {:?})...", storage.get_type()));
        storage.create(&name).await?;
    }

    // 2. Mount storage (returns rootfs path)
    let rootfs = if !is_ext {
        push_log("Mounting storage...".to_string());
        storage.mount(&name).await?
    } else {
        // For clones, machinectl clone handles everything. 
        // We use a dummy path since it won't be used for post-config anyway (skipped below).
        std::path::PathBuf::from(format!("/var/lib/machines/{}", name))
    };

    // Use a scoped guard-like pattern to ensure unmount
    let result = async {
        // 3. Perform base deployment
        deployer.deploy(&name, &cfg, &rootfs, logs.clone()).await?;

        // 4. Post-deployment configuration (skipped for clones as they are already configured)
        if is_ext {
             return Ok(());
        }

        if let Some(pwd) = &cfg.root_password {
            push_log("Setting root password...".to_string());
            crate::nspawn::create::set_root_password(&rootfs, pwd)
                .await?;
        }

        for user in &cfg.users {
            push_log(format!("Creating user {}...", user.username));
            crate::nspawn::create::create_user_in_container(&rootfs, user)
                .await?;
            
            if cfg.wayland_socket {
                push_log(format!("Setting up wayland env for {}...", user.username));
                crate::nspawn::create::setup_wayland_shell_env(&rootfs, user)
                    .await?;
            }
        }

        push_log("Writing .nspawn config...".to_string());
        crate::nspawn::create::write_nspawn_config(&cfg)?;

        if !cfg.device_binds.is_empty() {
            push_log("Writing systemd service override...".to_string());
            crate::nspawn::create::write_systemd_override(&name, &cfg.device_binds)?;
        }

        if let Some(mode) = &cfg.network {
            use crate::nspawn::models::NetworkMode;
            if matches!(mode, NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)) {
                push_log("Enabling container network (systemd-networkd)...".to_string());
                if let Err(e) = crate::nspawn::create::enable_container_networkd(&rootfs).await {
                    push_log(format!("WARNING: {} (might not be a systemd container)", e));
                }
            }
        }
        Ok::<(), NspawnError>(())
    }.await;

    // 5. Unmount storage
    if !is_ext {
        push_log("Unmounting storage...".to_string());
        if let Err(e) = storage.unmount(&name).await {
            push_log(format!("WARNING: Failed to unmount: {}", e));
        }
    }

    if let Err(e) = result {
        return Err(e);
    }

    push_log("".into());
    push_log("=== Deployment Complete ===".to_string());
    Ok(())
}

