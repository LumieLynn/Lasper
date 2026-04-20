use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
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

    crate::nspawn::sys::io::AsyncLockedWriter::write_locked(&path, |_| Ok(content)).await?;
    Ok(())
}

pub async fn save_internal_state(name: &str, state: &NvidiaState) -> Result<()> {
    let content = serde_json::to_string_pretty(state)?;

    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;
    let path = rootfs.join("etc/.lasper-nvidia.json");

    let res = crate::nspawn::sys::io::AsyncLockedWriter::write_locked(&path, |_| Ok(content)).await;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_paths_combines_both() {
        let state = NvidiaState {
            driver_version: "1.0".to_string(),
            readonly_binds: vec!["/ro1".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        let paths = state.all_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/ro1".to_string()));
        assert!(paths.contains(&"/dev1".to_string()));
    }

    #[test]
    fn test_all_paths_empty_state() {
        let state = NvidiaState::default();
        assert!(state.all_paths().is_empty());
    }

    #[test]
    fn test_calculate_death_list_removed_paths() {
        let old = NvidiaState {
            driver_version: "1.0".to_string(),
            readonly_binds: vec!["/ro1".to_string(), "/ro2".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        let new = NvidiaState {
            driver_version: "2.0".to_string(),
            readonly_binds: vec!["/ro1".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list, vec!["/ro2".to_string()]);
    }

    #[test]
    fn test_calculate_death_list_no_change() {
        let state = NvidiaState {
            driver_version: "1.0".to_string(),
            readonly_binds: vec!["/ro1".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        assert!(calculate_death_list(&state, &state).is_empty());
    }

    #[test]
    fn test_calculate_death_list_completely_new() {
        let old = NvidiaState::default();
        let new = NvidiaState {
            driver_version: "1.0".to_string(),
            readonly_binds: vec!["/ro1".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        // Nothing in old → nothing to kill
        assert!(calculate_death_list(&old, &new).is_empty());
    }

    #[test]
    fn test_calculate_death_list_everything_removed() {
        let old = NvidiaState {
            driver_version: "1.0".to_string(),
            readonly_binds: vec!["/ro1".to_string()],
            device_binds: vec!["/dev1".to_string()],
        };
        let new = NvidiaState::default();
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list.len(), 2);
        assert!(death_list.contains(&"/ro1".to_string()));
        assert!(death_list.contains(&"/dev1".to_string()));
    }

    #[test]
    fn test_nvidia_state_serde_roundtrip() {
        let state = NvidiaState {
            driver_version: "550.1".to_string(),
            readonly_binds: vec!["/usr/lib/libcuda.so".to_string()],
            device_binds: vec!["/dev/nvidia0".to_string()],
        };
        let serialized = serde_json::to_string(&state).unwrap();
        let deserialized: NvidiaState = serde_json::from_str(&serialized).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn test_nvidia_state_serde_empty_state() {
        let state = NvidiaState::default();
        let json = serde_json::to_string(&state).unwrap();
        let back: NvidiaState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.driver_version, "");
        assert!(back.readonly_binds.is_empty());
        assert!(back.device_binds.is_empty());
    }

    #[test]
    fn test_get_state_dir_respects_env() {
        let original = std::env::var("LASPER_STATE_DIR").ok();
        std::env::set_var("LASPER_STATE_DIR", "/custom/path");
        assert_eq!(get_state_dir(), PathBuf::from("/custom/path"));
        // Restore
        match original {
            Some(v) => std::env::set_var("LASPER_STATE_DIR", v),
            None => std::env::remove_var("LASPER_STATE_DIR"),
        }
    }
}
