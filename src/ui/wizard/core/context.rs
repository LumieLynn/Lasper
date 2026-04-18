use crate::nspawn::ops::provision::Deployer;
use crate::nspawn::models::{BindMount, CreateUser, NetworkMode, PortForward};
use crate::nspawn::adapters::storage::{StorageBackend, StorageInfo, StorageType};
use crate::nspawn::models::ContainerEntry;
pub use crate::nspawn::adapters::config::builder::{
    BasicConfig, ContainerConfigBuilder, ContainerConfigWithPreview, NetworkConfig,
    PassthroughConfig, SourceConfig, SourceKind, StorageConfig, UserConfig,
};
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub kind: SourceKind,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub bootstrap_pkgs: String,
    pub local_path: String,
    pub clone_source: String,
    pub pull_url: String,
    pub is_pull_raw: bool,
    pub copy_idx: usize,
}

impl SourceState {
    pub fn extract_config(&self) -> SourceConfig {
        SourceConfig {
            kind: self.kind,
            oci_url: self.oci_url.clone(),
            deboot_mirror: self.deboot_mirror.clone(),
            deboot_suite: self.deboot_suite.clone(),
            bootstrap_pkgs: self.bootstrap_pkgs.clone(),
            local_path: self.local_path.clone(),
            clone_source: self.clone_source.clone(),
            pull_url: self.pull_url.clone(),
            is_pull_raw: self.is_pull_raw,
        }
    }

    pub fn is_storage_managed_externally(&self) -> bool {
        match self.kind {
            SourceKind::Pull => self.is_pull_raw,
            SourceKind::LocalFile => {
                let p = self.local_path.to_lowercase();
                !(p.ends_with(".tar")
                    || p.ends_with(".tar.gz")
                    || p.ends_with(".tar.xz")
                    || p.ends_with(".tar.zst")
                    || p.ends_with(".tgz"))
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BasicState {
    pub name: String,
    pub hostname: String,
}

impl BasicState {
    pub fn extract_config(&self) -> BasicConfig {
        BasicConfig {
            name: self.name.clone(),
            hostname: self.hostname.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageState {
    pub type_idx: usize,
    pub info: StorageInfo,
    pub creation_method_idx: usize, // 0: Create New, 1: Import Existing
    pub disk_size: String,
    pub disk_fs: String,
    pub disk_partition: bool,
    pub import_path: String,
}

impl StorageState {
    pub fn extract_config(&self) -> StorageConfig {
        let (st, _) = self.info.types[self.type_idx].clone();
        StorageConfig {
            storage_type: st,
            disk_config: if st == StorageType::DiskImage {
                let source = if self.creation_method_idx == 1 {
                    crate::nspawn::models::DiskImageSource::ImportExisting {
                        path: self.import_path.clone(),
                    }
                } else {
                    crate::nspawn::models::DiskImageSource::CreateNew {
                        size: self.disk_size.clone(),
                        fs_type: self.disk_fs.clone(),
                    }
                };

                Some(crate::nspawn::models::DiskImageConfig {
                    source,
                    use_partition_table: self.disk_partition,
                })
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserState {
    pub root_password: String,
    pub users: Vec<CreateUser>,
}

impl UserState {
    pub fn extract_config(&self) -> UserConfig {
        UserConfig {
            root_password: if self.root_password.is_empty() {
                None
            } else {
                Some(self.root_password.clone())
            },
            users: self.users.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkState {
    pub mode: usize,
    pub bridge_name: String,
    pub bridge_list: Vec<String>,
    pub interface_name: String,
    pub physical_interfaces: Vec<String>,
    pub port_list: Vec<PortForward>,
}

impl NetworkState {
    pub fn network_mode(&self) -> Option<NetworkMode> {
        match self.mode {
            0 => Some(NetworkMode::Host),
            1 => Some(NetworkMode::None),
            2 => Some(NetworkMode::Veth),
            3 => Some(NetworkMode::Bridge(self.bridge_name.clone())),
            4 => Some(NetworkMode::MacVlan(self.interface_name.clone())),
            5 => Some(NetworkMode::IpVlan(self.interface_name.clone())),
            6 => Some(NetworkMode::Interface(self.interface_name.clone())),
            _ => None,
        }
    }

    pub fn extract_config(&self) -> NetworkConfig {
        NetworkConfig {
            mode: self.network_mode(),
            port_forwards: self.port_list.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassthroughState {
    pub privileged: bool,
    pub graphics_acceleration: bool,
    pub wayland_socket: Option<String>,
    pub discovered_gpus: Vec<crate::nspawn::platform::gpu::GpuDevice>,
    pub nvidia_gpu: bool,
    pub nvidia_toolkit_installed: bool,
    pub selected_gpu_nodes: Vec<String>,
    pub wayland_sockets: Vec<String>,
    pub bind_mounts: Vec<BindMount>,
}

impl PassthroughState {
    pub fn extract_config(&self, mode: Option<NetworkMode>) -> PassthroughConfig {
        let is_host_nw = matches!(mode, Some(NetworkMode::Host));
        PassthroughConfig {
            bind_mounts: self.bind_mounts.clone(),
            device_binds: self.selected_gpu_nodes.clone(),
            privileged: self.privileged,
            graphics_acceleration: self.graphics_acceleration,
            wayland_socket: if is_host_nw {
                self.wayland_socket.clone()
            } else {
                None
            },
            nvidia_gpu: self.nvidia_gpu && self.nvidia_toolkit_installed,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewState {
    pub preview: String,
}

#[derive(Clone)]
pub struct DeployState {
    pub log_tx: broadcast::Sender<String>,
    pub done: Arc<AtomicBool>,
    pub success: Arc<AtomicBool>,
}

impl PartialEq for DeployState {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for DeployState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeployState")
            .field("done", &self.done)
            .field("success", &self.success)
            .finish_non_exhaustive()
    }
}

/// Holds shared data for the multi-step container creation wizard.
#[derive(Debug, Clone, PartialEq)]
pub struct WizardContext {
    pub source: SourceState,
    pub basic: BasicState,
    pub storage: StorageState,
    pub user: UserState,
    pub network: NetworkState,
    pub passthrough: PassthroughState,
    pub review: ReviewState,
    pub deploy: DeployState,
    pub entries: Vec<ContainerEntry>,
    pub xdg_runtime: Option<String>,
}

impl WizardContext {
    pub async fn new(entries: Vec<ContainerEntry>) -> Self {
        let xdg_runtime = crate::nspawn::platform::capabilities::get_xdg_runtime().await.ok();
        let nvidia_toolkit_installed = tokio::fs::try_exists("/usr/bin/nvidia-ctk")
            .await
            .unwrap_or(false);
        let wayland_sockets = crate::nspawn::platform::capabilities::scan_available_wayland_sockets().await;
        let discovered_gpus = crate::nspawn::platform::gpu::discover_host_gpus().await;
        Self {
            source: SourceState {
                kind: SourceKind::Copy,
                oci_url: "".to_string(),
                deboot_mirror: "".to_string(),
                deboot_suite: "".to_string(),
                bootstrap_pkgs: "".to_string(),
                local_path: "".to_string(),
                clone_source: entries.first().map(|e| e.name.clone()).unwrap_or_default(),
                pull_url: "".to_string(),
                is_pull_raw: false,
                copy_idx: 0,
            },
            basic: BasicState {
                name: "".to_string(),
                hostname: "".to_string(),
            },
            storage: StorageState {
                type_idx: 0,
                info: crate::nspawn::adapters::storage::detect::detect_available_storage_types().await,
                creation_method_idx: 0,
                disk_size: "2G".to_string(),
                disk_fs: "ext4".to_string(),
                disk_partition: false,
                import_path: "".to_string(),
            },
            user: UserState {
                root_password: "".to_string(),
                users: vec![],
            },
            network: {
                let bridges = crate::nspawn::platform::network::detect_bridges().await;
                let default_bridge = bridges
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "br0".to_string());

                let physical_interfaces = crate::nspawn::platform::network::detect_physical_interfaces().await;
                let default_interface = physical_interfaces
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "eth0".to_string());

                NetworkState {
                    mode: 0,
                    bridge_name: default_bridge,
                    bridge_list: bridges,
                    interface_name: default_interface,
                    physical_interfaces,
                    port_list: vec![],
                }
            },
            passthrough: PassthroughState {
                privileged: false,
                graphics_acceleration: false,
                wayland_socket: None,
                discovered_gpus,
                nvidia_gpu: false,
                nvidia_toolkit_installed,
                selected_gpu_nodes: vec![],
                wayland_sockets,
                bind_mounts: vec![],
            },
            review: ReviewState {
                preview: "".to_string(),
            },
            deploy: {
                let (log_tx, _) = broadcast::channel(1000);
                DeployState {
                    log_tx,
                    done: Arc::new(AtomicBool::new(false)),
                    success: Arc::new(AtomicBool::new(false)),
                }
            },
            entries,
            xdg_runtime,
        }
    }

    pub fn builder(&self) -> ContainerConfigBuilder {
        ContainerConfigBuilder {
            source: Some(self.source.extract_config()),
            basic: Some(self.basic.extract_config()),
            storage: Some(self.storage.extract_config()),
            user: Some(self.user.extract_config()),
            network: Some(self.network.extract_config()),
            passthrough: Some(self.passthrough.extract_config(self.network.network_mode())),
        }
    }

    pub fn build_config(&self) -> ContainerConfigWithPreview {
        self.builder().build_config(self.xdg_runtime.as_deref())
    }

    pub fn build_preview_nspawn(&self) -> String {
        self.build_config().preview
    }

    pub fn get_deployer_and_storage(&self) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        self.builder().get_deployer_and_storage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_state_mode_mapping() {
        let mut state = NetworkState {
            mode: 0,
            bridge_name: "br0".into(),
            bridge_list: vec![],
            interface_name: "eth0".into(),
            physical_interfaces: vec![],
            port_list: vec![],
        };
        assert_eq!(state.network_mode(), Some(NetworkMode::Host));
        state.mode = 1;
        assert_eq!(state.network_mode(), Some(NetworkMode::None));
        state.mode = 2;
        assert_eq!(state.network_mode(), Some(NetworkMode::Veth));
        state.mode = 3;
        assert_eq!(state.network_mode(), Some(NetworkMode::Bridge("br0".into())));
    }

    #[test]
    fn test_source_state_externally_managed() {
        let mut state = SourceState {
            kind: SourceKind::Pull,
            oci_url: "".into(),
            deboot_mirror: "".into(),
            deboot_suite: "".into(),
            bootstrap_pkgs: "".into(),
            local_path: "".into(),
            clone_source: "".into(),
            pull_url: "".into(),
            is_pull_raw: true,
            copy_idx: 0,
        };
        assert!(state.is_storage_managed_externally());
        
        state.kind = SourceKind::LocalFile;
        state.local_path = "test.raw".into();
        assert!(state.is_storage_managed_externally());

        state.local_path = "test.tar.gz".into();
        assert!(!state.is_storage_managed_externally());
    }

    #[test]
    fn test_passthrough_config_logic() {
        let state = PassthroughState {
            privileged: true,
            graphics_acceleration: true,
            wayland_socket: Some("wayland-0".into()),
            discovered_gpus: vec![],
            nvidia_gpu: true,
            nvidia_toolkit_installed: true,
            selected_gpu_nodes: vec![],
            wayland_sockets: vec![],
            bind_mounts: vec![],
        };
        
        // Wayland only if Host network
        let cfg = state.extract_config(Some(NetworkMode::Host));
        assert!(cfg.wayland_socket.is_some());
        
        let cfg = state.extract_config(Some(NetworkMode::Veth));
        assert!(cfg.wayland_socket.is_none());

        // Nvidia GPU only if toolkit installed
        let mut state_no_toolkit = state.clone();
        state_no_toolkit.nvidia_toolkit_installed = false;
        let cfg = state_no_toolkit.extract_config(Some(NetworkMode::Host));
        assert!(!cfg.nvidia_gpu);
    }
}
