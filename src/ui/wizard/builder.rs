use crate::nspawn::create::{nspawn_config_content, systemd_override_content};
use crate::nspawn::deploy::Deployer;
use crate::nspawn::models::{BindMount, ContainerConfig, CreateUser, NetworkMode, PortForward};
use crate::nspawn::storage::{StorageBackend, StorageType};

/// The different methods available for acquiring a rootfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SourceKind {
    Copy,
    Oci,
    Debootstrap,
    Pacstrap,
    DiskImage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    pub kind: SourceKind,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub pacstrap_pkgs: String,
    pub disk_path: String,
    pub clone_source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BasicConfig {
    pub name: String,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub storage_type: StorageType,
    pub raw_config: Option<crate::nspawn::models::RawStorageConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserConfig {
    pub root_password: Option<String>,
    pub users: Vec<CreateUser>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkConfig {
    pub mode: Option<NetworkMode>,
    pub port_forwards: Vec<PortForward>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassthroughConfig {
    pub bind_mounts: Vec<BindMount>,
    pub device_binds: Vec<String>,
    pub full_capabilities: bool,
    pub wayland_socket: bool,
    pub nvidia_gpu: bool,
}

#[derive(Default, Clone)]
pub struct ContainerConfigBuilder {
    pub source: Option<SourceConfig>,
    pub basic: Option<BasicConfig>,
    pub storage: Option<StorageConfig>,
    pub user: Option<UserConfig>,
    pub network: Option<NetworkConfig>,
    pub passthrough: Option<PassthroughConfig>,
}

impl ContainerConfigBuilder {
    pub fn build_config(&self) -> ContainerConfigWithPreview {
        let passthrough = self
            .passthrough
            .as_ref()
            .cloned()
            .unwrap_or(PassthroughConfig {
                bind_mounts: vec![],
                device_binds: vec![],
                full_capabilities: false,
                wayland_socket: false,
                nvidia_gpu: false,
            });

        let basic = self.basic.as_ref().cloned().unwrap_or(BasicConfig {
            name: "unknown".to_string(),
            hostname: "unknown".to_string(),
        });

        let nw = self.network.as_ref().cloned().unwrap_or(NetworkConfig {
            mode: Some(NetworkMode::Host),
            port_forwards: vec![],
        });

        let storage = self.storage.as_ref().cloned().unwrap_or(StorageConfig {
            storage_type: StorageType::Directory,
            raw_config: None,
        });

        let user = self.user.as_ref().cloned().unwrap_or(UserConfig {
            root_password: None,
            users: vec![],
        });

        let cfg = ContainerConfig {
            name: basic.name.clone(),
            hostname: basic.hostname.clone(),
            network: nw.mode.clone(),
            port_forwards: nw.port_forwards.clone(),
            bind_mounts: passthrough.bind_mounts.clone(),
            device_binds: passthrough.device_binds.clone(),
            readonly_binds: vec![],
            full_capabilities: passthrough.full_capabilities,
            root_password: user.root_password.clone(),
            users: user.users.clone(),
            wayland_socket: passthrough.wayland_socket,
            nvidia_gpu: passthrough.nvidia_gpu,
            raw_config: storage.raw_config.clone(),
        };

        if let Some(source) = &self.source {
            if source.kind == SourceKind::Copy {
                let mut content = format!(
                    " [CLONE OPERATION]\n\n Source: {}\n Destination: {}\n\n",
                    source.clone_source, basic.name
                );
                content.push_str(" All configuration files (.nspawn) and systemd service\n overrides will be copied automatically.");
                return ContainerConfigWithPreview {
                    cfg,
                    preview: content,
                };
            }
        }

        let mut content = format!(" [DEPLOYMENT PREVIEW — {}]\n\n", basic.name);
        content.push_str(&format!(
            " Storage: {} ({})\n",
            storage.storage_type.label(),
            storage.storage_type.get_path(&basic.name).display()
        ));
        content.push_str(&format!(" Hostname: {}\n", cfg.hostname));
        content.push_str(&nspawn_config_content(&cfg));
        if !cfg.device_binds.is_empty() || cfg.nvidia_gpu {
            content.push_str("\n# ── [systemd override.conf] ───────────────────────────\n");
            content.push_str(&systemd_override_content(&cfg.device_binds, cfg.nvidia_gpu));
        }

        ContainerConfigWithPreview {
            cfg,
            preview: content,
        }
    }

    pub fn get_deployer_and_storage(&self) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        use crate::nspawn::deploy::*;
        use crate::nspawn::storage::*;

        let storage_cfg = self.storage.as_ref().cloned().unwrap_or(StorageConfig {
            storage_type: StorageType::Directory,
            raw_config: None,
        });

        let storage: Box<dyn StorageBackend> = match storage_cfg.storage_type {
            StorageType::Directory => Box::new(DirectoryBackend),
            StorageType::Subvolume => Box::new(SubvolumeBackend),
            StorageType::Raw => Box::new(RawBackend {
                config: storage_cfg
                    .raw_config
                    .unwrap_or(crate::nspawn::models::RawStorageConfig {
                        size: "2G".to_string(),
                        fs_type: "ext4".to_string(),
                        use_partition_table: false,
                    }),
            }),
        };

        let source = self.source.as_ref().cloned().unwrap_or(SourceConfig {
            kind: SourceKind::Oci,
            oci_url: String::new(),
            deboot_mirror: String::new(),
            deboot_suite: String::new(),
            pacstrap_pkgs: String::new(),
            disk_path: String::new(),
            clone_source: String::new(),
        });

        let deployer: Box<dyn Deployer> = match source.kind {
            SourceKind::Copy => Box::new(clone::CloneDeployer {
                source_name: source.clone_source.clone(),
            }),
            SourceKind::Oci => Box::new(image::OciDeployer {
                url: source.oci_url.clone(),
            }),
            SourceKind::DiskImage => Box::new(image::DiskImageDeployer {
                path: source.disk_path.clone(),
            }),
            SourceKind::Debootstrap => Box::new(bootstrap::DebootstrapDeployer {
                mirror: source.deboot_mirror.clone(),
                suite: if source.deboot_suite.is_empty() {
                    "bookworm".to_string()
                } else {
                    source.deboot_suite.clone()
                },
            }),
            SourceKind::Pacstrap => Box::new(bootstrap::PacstrapDeployer {
                packages: source.pacstrap_pkgs.clone(),
            }),
        };

        (deployer, storage)
    }
}

pub struct ContainerConfigWithPreview {
    pub cfg: ContainerConfig,
    pub preview: String,
}
