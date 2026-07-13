use crate::nspawn::errors::Result;
use crate::nspawn::platform::nvidia::classify::{ClassifiedEntry, SymlinkEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// A single host→container path mapping for an nspawn Bind= or BindReadOnly= entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PassthroughBind {
    pub host_path: String,
    pub container_path: String,
    /// false = Bind (rw, device nodes), true = BindReadOnly (ro, libraries/configs)
    pub readonly: bool,
}

/// Hardware and driver information detected on the host for mounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NvidiaState {
    /// Host driver version when this state was captured.
    pub driver_version: String,

    /// Unified bind-mount entries. This is the primary field.
    #[serde(default)]
    pub binds: Vec<PassthroughBind>,

    // Legacy fields — kept for backward compat with old state files.
    // New state files omit these via skip_serializing_if.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_binds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_binds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classified_entries: Vec<ClassifiedEntry>,

    #[allow(dead_code)]
    #[serde(default)]
    pub symlinks: Vec<SymlinkEntry>,
    #[serde(default)]
    pub ldcache_folders: Vec<String>,
    #[serde(default)]
    pub env_vars: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
}

impl NvidiaState {
    /// Populate `binds` from legacy fields if binds is empty and legacy fields have data.
    /// Called after deserialization of old-format state files.
    pub fn migrate_from_legacy(&mut self) {
        if !self.binds.is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        for path in &self.device_binds {
            let (host, container) = split_host_container(path);
            let key = format!("{}::{}", host, container);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: host,
                    container_path: container,
                    readonly: false,
                });
            }
        }
        for path in &self.readonly_binds {
            let (host, container) = split_host_container(path);
            let key = format!("{}::{}", host, container);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: host,
                    container_path: container,
                    readonly: true,
                });
            }
        }
        for ce in &self.classified_entries {
            let key = format!("{}::{}", ce.host_path, ce.default_container_path);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: ce.host_path.clone(),
                    container_path: ce.default_container_path.clone(),
                    readonly: true,
                });
            }
        }
        for sym in &self.symlinks {
            let key = format!("{}::{}", sym.target, sym.link_path);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: sym.target.clone(),
                    container_path: sym.link_path.clone(),
                    readonly: true,
                });
            }
        }
    }

    /// Extract legacy fields from `binds` for backward compat consumers.
    pub fn populate_legacy(&mut self) {
        self.readonly_binds.clear();
        self.device_binds.clear();
        // classified_entries and symlinks are managed separately
        // (extract_classified_entries), do not clear them here

        for bind in &self.binds {
            if bind.readonly {
                if bind.host_path == bind.container_path {
                    self.readonly_binds.push(bind.host_path.clone());
                } else {
                    self.readonly_binds
                        .push(format!("{}:{}", bind.host_path, bind.container_path));
                }
            } else {
                self.device_binds.push(bind.host_path.clone());
            }
        }
    }

    /// Returns all host paths involved in this state.
    #[allow(dead_code)]
    pub fn all_host_paths(&self) -> Vec<String> {
        let mut paths = HashSet::new();
        for b in &self.binds {
            paths.insert(b.host_path.clone());
        }
        // Also check legacy for code that hasn't migrated yet
        for b in &self.readonly_binds {
            paths.insert(b.split(':').next().unwrap_or(b).to_string());
        }
        for b in &self.device_binds {
            paths.insert(b.split(':').next().unwrap_or(b).to_string());
        }
        for e in &self.classified_entries {
            paths.insert(e.host_path.clone());
        }
        paths.into_iter().collect()
    }

    /// Returns all container paths that were created/mounted.
    pub fn all_container_paths(&self) -> Vec<String> {
        let mut paths = HashSet::new();
        for b in &self.binds {
            paths.insert(b.container_path.clone());
        }
        // Also check legacy for backward compat
        for b in &self.readonly_binds {
            paths.insert(b.split(':').next_back().unwrap_or(b).to_string());
        }
        for b in &self.device_binds {
            paths.insert(b.split(':').next_back().unwrap_or(b).to_string());
        }
        for e in &self.classified_entries {
            paths.insert(e.default_container_path.clone());
        }
        for s in &self.symlinks {
            paths.insert(s.link_path.clone());
        }
        paths.into_iter().collect()
    }

    /// Backward compatibility for legacy code that expects flattened paths.
    pub fn all_paths(&self) -> Vec<String> {
        self.all_container_paths()
    }
}

/// Splits "host:container" or "path" into (host, container).
fn split_host_container(s: &str) -> (String, String) {
    s.split_once(':')
        .map(|(h, c)| (h.to_string(), c.to_string()))
        .unwrap_or((s.to_string(), s.to_string()))
}

#[allow(dead_code)]
pub(crate) fn get_state_dir() -> PathBuf {
    crate::paths::state_dir()
}

pub async fn get_external_state(
    name: &str,
    io: &crate::nspawn::sys::ElevatedIo,
) -> Result<Option<NvidiaState>> {
    let path = crate::paths::state_file(name);
    let content = match io.read_to_string(&path).await? {
        Some(c) => c,
        None => return Ok(None),
    };
    let mut state: NvidiaState = serde_json::from_str(&content)?;
    state.migrate_from_legacy();
    Ok(Some(state))
}

pub async fn save_external_state(
    name: &str,
    state: &NvidiaState,
    io: &crate::nspawn::sys::ElevatedIo,
) -> Result<()> {
    let path = crate::paths::state_file(name);
    let content = serde_json::to_string_pretty(state)?;

    io.write(&path, &content).await?;
    Ok(())
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
    fn test_passthrough_bind_serde_roundtrip() {
        let bind = PassthroughBind {
            host_path: "/host/lib/libcuda.so".into(),
            container_path: "/usr/lib/libcuda.so".into(),
            readonly: true,
        };
        let json = serde_json::to_string(&bind).unwrap();
        let back: PassthroughBind = serde_json::from_str(&json).unwrap();
        assert_eq!(bind, back);
    }

    #[test]
    fn test_migrate_empty_state_noop() {
        let mut state = NvidiaState::default();
        state.migrate_from_legacy();
        assert!(state.binds.is_empty());
    }

    #[test]
    fn test_migrate_already_populated_noop() {
        let mut state = NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/host/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            readonly_binds: vec!["/old/path".into()],
            ..Default::default()
        };
        state.migrate_from_legacy();
        assert_eq!(state.binds.len(), 1);
    }

    #[test]
    fn test_migrate_from_legacy_all_fields() {
        let mut state = NvidiaState {
            driver_version: "1.0".into(),
            device_binds: vec!["/dev/nvidia0".into()],
            readonly_binds: vec!["/usr/lib/libcuda.so:/usr/lib/libcuda.so".into()],
            classified_entries: vec![ClassifiedEntry {
                host_path: "/host/gsp.bin".into(),
                default_container_path: "/lib/firmware/nvidia/gsp.bin".into(),
                category: crate::nspawn::platform::nvidia::classify::NvidiaFileCategory::Firmware,
            }],
            symlinks: vec![SymlinkEntry {
                target: "/usr/lib/libcuda.so.1".into(),
                link_path: "/usr/lib/libcuda.so".into(),
            }],
            ..Default::default()
        };
        state.migrate_from_legacy();

        // device_binds → readonly:false
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/dev/nvidia0" && !b.readonly));
        // readonly_binds with host:container
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so"
                && b.container_path == "/usr/lib/libcuda.so"
                && b.readonly));
        // classified_entries
        assert!(state.binds.iter().any(|b| b.host_path == "/host/gsp.bin"
            && b.container_path == "/lib/firmware/nvidia/gsp.bin"));
        // symlinks
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so.1"
                && b.container_path == "/usr/lib/libcuda.so"
                && b.readonly));
    }

    #[test]
    fn test_migrate_dedup_same_path() {
        let mut state = NvidiaState {
            device_binds: vec!["/dev/nvidia0".into()],
            readonly_binds: vec!["/dev/nvidia0".into()], // same host path as device
            ..Default::default()
        };
        state.migrate_from_legacy();
        // Only one entry per unique (host, container) pair
        let dev_entries: Vec<_> = state
            .binds
            .iter()
            .filter(|b| b.host_path == "/dev/nvidia0")
            .collect();
        assert_eq!(dev_entries.len(), 1);
    }

    #[test]
    fn test_populate_legacy_from_binds() {
        let state = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };
        let mut clone = state.clone();
        clone.populate_legacy();
        assert!(clone.device_binds.contains(&"/dev/nvidia0".to_string()));
        assert!(clone
            .readonly_binds
            .contains(&"/host/libcuda.so:/usr/lib/libcuda.so".to_string()));
    }

    #[test]
    fn test_all_paths_from_binds() {
        let state = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };
        let paths = state.all_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/dev/nvidia0".to_string()));
        assert!(paths.contains(&"/usr/lib/libcuda.so".to_string()));
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
            binds: vec![
                PassthroughBind {
                    host_path: "/ro1".into(),
                    container_path: "/ro1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/ro2".into(),
                    container_path: "/ro2".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev1".into(),
                    container_path: "/dev1".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let new = NvidiaState {
            driver_version: "2.0".to_string(),
            binds: vec![
                PassthroughBind {
                    host_path: "/ro1".into(),
                    container_path: "/ro1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev1".into(),
                    container_path: "/dev1".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list, vec!["/ro2".to_string()]);
    }

    #[test]
    fn test_calculate_death_list_no_change() {
        let state = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        assert!(calculate_death_list(&state, &state).is_empty());
    }

    #[test]
    fn test_calculate_death_list_completely_new() {
        let old = NvidiaState::default();
        let new = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        assert!(calculate_death_list(&old, &new).is_empty());
    }

    #[test]
    fn test_calculate_death_list_everything_removed() {
        let old = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        let new = NvidiaState::default();
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list, vec!["/ro1".to_string()]);
    }

    #[test]
    fn test_nvidia_state_serde_roundtrip() {
        let state = NvidiaState {
            driver_version: "550.1".to_string(),
            binds: vec![
                PassthroughBind {
                    host_path: "/usr/lib/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
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
        assert!(back.binds.is_empty());
        assert!(back.readonly_binds.is_empty());
        assert!(back.device_binds.is_empty());
    }

    #[test]
    fn test_nvidia_state_serde_legacy_migration() {
        // Simulate an old-format state file (no "binds" key)
        let json = r#"{
            "driver_version": "550.1",
            "readonly_binds": ["/usr/lib/libcuda.so"],
            "device_binds": ["/dev/nvidia0"],
            "classified_entries": [],
            "symlinks": [],
            "ldcache_folders": [],
            "env_vars": []
        }"#;
        let mut state: NvidiaState = serde_json::from_str(json).unwrap();
        assert!(state.binds.is_empty());
        state.migrate_from_legacy();
        assert_eq!(state.binds.len(), 2);
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/dev/nvidia0" && !b.readonly));
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so" && b.readonly));
    }

    #[test]
    fn test_get_state_dir_respects_env() {
        let original = std::env::var("LASPER_STATE_DIR").ok();
        std::env::set_var("LASPER_STATE_DIR", "/custom/path");
        assert_eq!(get_state_dir(), PathBuf::from("/custom/path"));
        match original {
            Some(v) => std::env::set_var("LASPER_STATE_DIR", v),
            None => std::env::remove_var("LASPER_STATE_DIR"),
        }
    }
}
