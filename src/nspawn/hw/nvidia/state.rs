use crate::nspawn::errors::{NspawnError, Result};
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Hardware and driver information detected on the host for mounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NvidiaState {
    /// Host driver version when this state was captured.
    pub driver_version: String,
    /// Host paths to bind-mount into the container (read-only).
    pub readonly_binds: Vec<String>,
    /// Device files to bind-mount (read-write).
    pub device_binds: Vec<String>,
}

impl NvidiaState {
    pub fn all_paths(&self) -> Vec<String> {
        let mut paths = self.readonly_binds.clone();
        paths.extend(self.device_binds.clone());
        paths
    }
}

pub(crate) fn get_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LASPER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("lasper").join("states");
    }
    if let Some(home) = dirs::home_dir() {
        home.join(".local")
            .join("state")
            .join("lasper")
            .join("states")
    } else {
        PathBuf::from("/var/lib/lasper/states")
    }
}

pub async fn get_external_state(name: &str) -> Result<Option<NvidiaState>> {
    let dir = get_state_dir();
    let path = dir.join(format!("{}.json", name));
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| NspawnError::Io(path.clone(), e))?;
    let state: NvidiaState = serde_json::from_str(&content)?;
    Ok(Some(state))
}

pub async fn get_internal_state(name: &str) -> Result<Option<NvidiaState>> {
    let backend = crate::nspawn::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;
    let path = rootfs.join("etc/.lasper-nvidia.json");

    let res = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    let _ = backend.unmount(name).await;
    Ok(res)
}

pub async fn save_external_state(name: &str, state: &NvidiaState) -> Result<()> {
    let dir = get_state_dir();
    let path = dir.join(format!("{}.json", name));
    let content = serde_json::to_string_pretty(state)?;

    crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&path, |_| Ok(content)).await?;
    Ok(())
}

pub async fn save_internal_state(name: &str, state: &NvidiaState) -> Result<()> {
    let content = serde_json::to_string_pretty(state)?;

    let backend = crate::nspawn::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;
    let path = rootfs.join("etc/.lasper-nvidia.json");

    let res = crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&path, |_| Ok(content)).await;

    let _ = backend.unmount(name).await;
    res
}

pub(crate) fn calculate_death_list(old: &NvidiaState, new: &NvidiaState) -> Vec<String> {
    let old_paths = old.all_paths();
    let new_paths = new.all_paths();
    old_paths
        .into_iter()
        .filter(|p| !new_paths.contains(p))
        .collect()
}
