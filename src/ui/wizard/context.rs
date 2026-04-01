use std::sync::{Arc, Mutex, atomic::AtomicBool};
use crate::nspawn::ContainerEntry;
use crate::nspawn::models::{CreateUser, BindMount, PortForward, NetworkMode};
use crate::nspawn::storage::{StorageBackend, StorageType, StorageInfo};
use crate::nspawn::deploy::Deployer;
pub use crate::ui::wizard::builder::{ContainerConfigBuilder, ContainerConfigWithPreview, SourceKind, SourceConfig, BasicConfig, StorageConfig, UserConfig, NetworkConfig, PassthroughConfig};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub kind: SourceKind,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub pacstrap_pkgs: String,
    pub disk_path: String,
    pub clone_source: String,
    pub copy_idx: usize,
}

impl SourceState {
    pub fn extract_config(&self) -> SourceConfig {
        SourceConfig {
            kind: self.kind,
            oci_url: self.oci_url.clone(),
            deboot_mirror: self.deboot_mirror.clone(),
            deboot_suite: self.deboot_suite.clone(),
            pacstrap_pkgs: self.pacstrap_pkgs.clone(),
            disk_path: self.disk_path.clone(),
            clone_source: self.clone_source.clone(),
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
    pub raw_size: String,
    pub raw_fs: String,
    pub raw_partition: bool,
}

impl StorageState {
    pub fn extract_config(&self) -> StorageConfig {
        let (st, _) = self.info.types[self.type_idx].clone();
        StorageConfig {
            storage_type: st,
            raw_config: if st == StorageType::Raw {
                Some(crate::nspawn::models::RawStorageConfig {
                    size: self.raw_size.clone(),
                    fs_type: self.raw_fs.clone(),
                    use_partition_table: self.raw_partition,
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
            root_password: if self.root_password.is_empty() { None } else { Some(self.root_password.clone()) },
            users: self.users.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkState {
    pub mode: usize,
    pub bridge_name: String,
    pub bridge_list: Vec<String>,
    pub port_list: Vec<PortForward>,
}

impl NetworkState {
    pub fn network_mode(&self) -> Option<NetworkMode> {
        match self.mode {
            0 => Some(NetworkMode::Host),
            1 => Some(NetworkMode::None),
            2 => Some(NetworkMode::Veth),
            3 => Some(NetworkMode::Bridge(self.bridge_name.clone())),
            _ => None,
        }
    }

    pub fn extract_config(&self) -> NetworkConfig {
        NetworkConfig {
            mode: self.network_mode(),
            port_forwards: self.port_list.clone(),
        }
    }

    pub fn parse_port(input: &str) -> Option<PortForward> {
        let parts: Vec<&str> = input.split(':').collect();
        if parts.len() < 2 { return None; }
        let host = parts[0].parse::<u16>().ok()?;
        let rest = parts[1];
        let (container_str, proto) = if rest.contains('/') {
            let p: Vec<&str> = rest.split('/').collect();
            (p[0], p[1].to_string())
        } else {
            (rest, "tcp".to_string())
        };
        let container = container_str.parse::<u16>().ok()?;
        Some(PortForward { host, container, proto })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassthroughState {
    pub full_capabilities: bool,
    pub wayland_socket: bool,
    pub nvidia_gpu: bool,
    pub nvidia_toolkit_installed: bool,
    pub bind_mounts: Vec<BindMount>,
}

impl PassthroughState {
    pub fn extract_config(&self, mode: Option<NetworkMode>) -> PassthroughConfig {
        let is_host_nw = matches!(mode, Some(NetworkMode::Host));
        PassthroughConfig {
            bind_mounts: self.bind_mounts.clone(),
            device_binds: vec![], // Managed by bridge or nvidia-ctk
            full_capabilities: self.full_capabilities,
            wayland_socket: self.wayland_socket && is_host_nw,
            nvidia_gpu: self.nvidia_gpu && self.nvidia_toolkit_installed,
        }
    }

    pub fn parse_bind_mount(input: &str) -> Option<BindMount> {
        let parts: Vec<&str> = input.split(':').collect();
        if parts.len() < 2 { return None; }
        let source = parts[0].to_string();
        let rest = parts[1];
        let (target, readonly) = if rest.contains(":ro") {
            (rest.replace(":ro", ""), true)
        } else if rest.contains(":rw") {
            (rest.replace(":rw", ""), false)
        } else {
            (rest.to_string(), false)
        };
        Some(BindMount { source, target, readonly })
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
    fn eq(&self, _other: &Self) -> bool { true }
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
}

impl WizardContext {
    pub fn new(entries: Vec<ContainerEntry>) -> Self {
        let nvidia_toolkit_installed = std::path::Path::new("/usr/bin/nvidia-ctk").exists();
        Self {
            source: SourceState {
                kind: SourceKind::Copy,
                oci_url: "".to_string(),
                deboot_mirror: "".to_string(),
                deboot_suite: "".to_string(),
                pacstrap_pkgs: "".to_string(),
                disk_path: "".to_string(),
                clone_source: entries.first().map(|e| e.name.clone()).unwrap_or_default(),
                copy_idx: 0,
            },
            basic: BasicState {
                name: "".to_string(),
                hostname: "".to_string(),
            },
            storage: StorageState {
                type_idx: 0,
                info: crate::nspawn::storage::detect_available_storage_types(),
                raw_size: "2G".to_string(),
                raw_fs: "ext4".to_string(),
                raw_partition: false,
            },
            user: UserState {
                root_password: "".to_string(),
                users: vec![],
            },
            network: {
                let bridges = crate::nspawn::network::detect_bridges();
                let default_bridge = bridges.first().cloned().unwrap_or_else(|| "br0".to_string());
                NetworkState {
                    mode: 0,
                    bridge_name: default_bridge,
                    bridge_list: bridges,
                    port_list: vec![],
                }
            },
            passthrough: PassthroughState {
                full_capabilities: false,
                wayland_socket: false,
                nvidia_gpu: false,
                nvidia_toolkit_installed,
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
        self.builder().build_config()
    }

    pub fn build_preview_nspawn(&self) -> String {
        self.build_config().preview
    }

    pub fn get_deployer_and_storage(&self) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        self.builder().get_deployer_and_storage()
    }
}
