use crate::nspawn::adapters::config::nspawn_file::nspawn_config_content;
use crate::nspawn::adapters::config::systemd_unit::systemd_override_content;
use crate::nspawn::adapters::storage::{StorageBackend, StorageType};
use crate::nspawn::models::{BindMount, ContainerConfig, CreateUser, NetworkMode, PortForward};
use crate::nspawn::ops::provision::builders::{bootstrap, clone, image};
use crate::nspawn::ops::provision::Deployer;

/// The different methods available for acquiring a rootfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SourceKind {
    Copy,
    Oci,
    Debootstrap,
    Pacstrap,
    Pull,
    LocalFile,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    pub kind: SourceKind,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub bootstrap_pkgs: String,
    pub local_path: String,
    pub clone_source: String,
    pub pull_url: String,
    pub is_pull_raw: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BasicConfig {
    pub name: String,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub storage_type: StorageType,
    pub disk_config: Option<crate::nspawn::models::DiskImageConfig>,
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
    pub privileged: bool,
    pub graphics_acceleration: bool,
    pub wayland_socket: Option<String>,
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
    pub fn build_config(&self, xdg_runtime: Option<&str>) -> ContainerConfigWithPreview {
        let passthrough = self
            .passthrough
            .as_ref()
            .cloned()
            .unwrap_or(PassthroughConfig {
                bind_mounts: vec![],
                device_binds: vec![],
                privileged: false,
                graphics_acceleration: false,
                wayland_socket: None,
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
            disk_config: None,
        });

        let user = self.user.as_ref().cloned().unwrap_or(UserConfig {
            root_password: None,
            users: vec![],
        });

        let device_binds = passthrough.device_binds.clone();

        let cfg = ContainerConfig {
            name: basic.name.clone(),
            hostname: basic.hostname.clone(),
            network: nw.mode.clone(),
            port_forwards: nw.port_forwards.clone(),
            bind_mounts: passthrough.bind_mounts.clone(),
            device_binds,
            readonly_binds: vec![],
            privileged: passthrough.privileged,
            graphics_acceleration: passthrough.graphics_acceleration,
            root_password: user.root_password.clone(),
            users: user.users.clone(),
            wayland_socket: passthrough.wayland_socket.clone(),
            nvidia_gpu: passthrough.nvidia_gpu,
            disk_config: storage.disk_config.clone(),
            boot: if let Some(s) = &self.source {
                s.kind != SourceKind::Oci
            } else {
                true
            },
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
        content.push_str(
            &nspawn_config_content(&cfg, xdg_runtime)
                .unwrap_or_else(|e| format!(" [ERROR: {}]", e)),
        );
        if !cfg.device_binds.is_empty()
            || cfg.nvidia_gpu
            || cfg.wayland_socket.is_some()
            || cfg.graphics_acceleration
        {
            content.push_str("\n# ── [systemd override.conf] ───────────────────────────\n");
            content.push_str(&systemd_override_content(
                &cfg.device_binds,
                cfg.nvidia_gpu,
                cfg.graphics_acceleration,
                cfg.wayland_socket.is_some(),
            ));
        }

        ContainerConfigWithPreview {
            cfg,
            preview: content,
        }
    }

    pub fn get_deployer_and_storage(&self) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        use crate::nspawn::adapters::storage::*;
        use crate::nspawn::ops::provision::*;

        let storage_cfg = self.storage.as_ref().cloned().unwrap_or(StorageConfig {
            storage_type: StorageType::Directory,
            disk_config: None,
        });

        let storage: Box<dyn StorageBackend> = match storage_cfg.storage_type {
            StorageType::Directory => Box::new(DirectoryBackend) as Box<dyn StorageBackend>,
            StorageType::Subvolume => Box::new(SubvolumeBackend) as Box<dyn StorageBackend>,
            StorageType::DiskImage => Box::new(DiskImageBackend {
                config: storage_cfg
                    .disk_config
                    .unwrap_or(crate::nspawn::models::DiskImageConfig {
                        source: crate::nspawn::models::DiskImageSource::CreateNew {
                            size: "2G".to_string(),
                            fs_type: "ext4".to_string(),
                        },
                        use_partition_table: false,
                    }),
            }) as Box<dyn StorageBackend>,
        };

        let source = self.source.as_ref().cloned().unwrap_or(SourceConfig {
            kind: SourceKind::Oci,
            oci_url: String::new(),
            deboot_mirror: String::new(),
            deboot_suite: String::new(),
            bootstrap_pkgs: String::new(),
            local_path: String::new(),
            clone_source: String::new(),
            pull_url: String::new(),
            is_pull_raw: false,
        });

        let deployer: Box<dyn Deployer> = match source.kind {
            SourceKind::Copy => Box::new(clone::CloneDeployer {
                source_name: source.clone_source.clone(),
            }) as Box<dyn Deployer>,
            SourceKind::Oci => Box::new(image::OciDeployer {
                url: source.oci_url.clone(),
            }) as Box<dyn Deployer>,
            SourceKind::LocalFile => Box::new(image::DiskImageDeployer {
                path: source.local_path.clone(),
            }) as Box<dyn Deployer>,
            SourceKind::Pull => Box::new(image::NetworkImageDeployer {
                url: source.pull_url.clone(),
                is_raw: source.is_pull_raw,
            }) as Box<dyn Deployer>,
            SourceKind::Debootstrap => Box::new(bootstrap::DebootstrapDeployer {
                mirror: source.deboot_mirror.clone(),
                suite: if source.deboot_suite.is_empty() {
                    "bookworm".to_string()
                } else {
                    source.deboot_suite.clone()
                },
                packages: source.bootstrap_pkgs.clone(),
            }) as Box<dyn Deployer>,
            SourceKind::Pacstrap => Box::new(bootstrap::PacstrapDeployer {
                packages: source.bootstrap_pkgs.clone(),
            }) as Box<dyn Deployer>,
        };

        (deployer, storage)
    }
}

pub struct ContainerConfigWithPreview {
    pub cfg: ContainerConfig,
    pub preview: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::NetworkMode;

    #[test]
    fn test_build_config_defaults() {
        let builder = ContainerConfigBuilder::default();
        let result = builder.build_config(None);
        assert_eq!(result.cfg.name, "unknown");
        assert_eq!(result.cfg.hostname, "unknown");
        assert_eq!(result.cfg.network, Some(NetworkMode::Host));
    }

    #[test]
    fn test_build_config_oci_disables_boot() {
        let mut builder = ContainerConfigBuilder::default();
        builder.source = Some(SourceConfig {
            kind: SourceKind::Oci,
            oci_url: "ubuntu".to_string(),
            deboot_mirror: "".to_string(),
            deboot_suite: "".to_string(),
            bootstrap_pkgs: "".to_string(),
            local_path: "".to_string(),
            clone_source: "".to_string(),
            pull_url: "".to_string(),
            is_pull_raw: false,
        });
        let result = builder.build_config(None);
        assert!(!result.cfg.boot);
    }

    #[test]
    fn test_build_config_passthrough_fields() {
        let mut builder = ContainerConfigBuilder::default();
        builder.passthrough = Some(PassthroughConfig {
            bind_mounts: vec![],
            device_binds: vec!["/dev/dri/card0".to_string()],
            privileged: true,
            graphics_acceleration: true,
            wayland_socket: Some("wayland-0".to_string()),
            nvidia_gpu: true,
        });
        let result = builder.build_config(None);
        assert!(result.cfg.privileged);
        assert!(result.cfg.graphics_acceleration);
        assert_eq!(result.cfg.device_binds, vec!["/dev/dri/card0"]);
        assert_eq!(result.cfg.wayland_socket, Some("wayland-0".to_string()));
        assert!(result.cfg.nvidia_gpu);
    }
}
