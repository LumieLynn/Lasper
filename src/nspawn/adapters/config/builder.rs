use crate::nspawn::adapters::config::nspawn_file::nspawn_config_content;
use crate::nspawn::adapters::config::systemd_unit::systemd_override_content;
use crate::nspawn::adapters::storage::{StorageBackend, StorageType};
use crate::nspawn::models::{ArtifactSpec, BootstrapMethod, BootstrapSpec};
use crate::nspawn::models::{BindMount, ContainerConfig, CreateUser, NetworkMode, PortForward};
use crate::nspawn::ops::provision::builders::{bootstrap, clone, image, oci};
use crate::nspawn::ops::provision::Deployer;

/// The different methods available for acquiring a rootfs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SourceKind {
    Copy,
    Oci,
    Debootstrap,
    Pacstrap,
    Dnf5,
    Pull,
    LocalFile,
    Profile {
        method: BootstrapMethod,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceConfig {
    Copy { source_name: String },
    Oci { reference: String, read_only: bool },
    Bootstrap(BootstrapSpec),
    Pull { url: String, is_raw: bool },
    Artifact(ArtifactSpec),
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
    pub private_users: Option<String>,
    pub graphics_acceleration: bool,
    pub wayland_socket: Option<String>,
    pub nvidia_gpu: bool,
    pub nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
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
                private_users: None,
                graphics_acceleration: false,
                wayland_socket: None,
                nvidia_gpu: false,
                nvidia_profile: None,
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
            private_users: passthrough.private_users,
            graphics_acceleration: passthrough.graphics_acceleration,
            root_password: user.root_password.clone(),
            users: user.users.clone(),
            wayland_socket: passthrough.wayland_socket.clone(),
            nvidia_gpu: passthrough.nvidia_gpu,
            disk_config: storage.disk_config.clone(),
            boot: !matches!(self.source, Some(SourceConfig::Oci { .. })),
        };

        if let Some(SourceConfig::Oci {
            reference,
            read_only,
        }) = &self.source
        {
            let mode = if *read_only {
                "read-only layers"
            } else {
                "writable overlay"
            };
            let preview = format!(
                " [SYSTEMD OCI APPLICATION]\n\n Reference: {reference}\n Name: {}\n Storage: /var/lib/machines/{}.mstack\n Mode: {mode}\n Runtime config: preserved from OCI image\n Verification: HTTPS transport authentication; no publisher signature verification\n",
                basic.name, basic.name
            );
            return ContainerConfigWithPreview {
                cfg,
                preview,
                nvidia_profile: None,
            };
        }

        if let Some(SourceConfig::Copy { source_name }) = &self.source {
            let mut content = format!(
                " [CLONE OPERATION]\n\n Source: {}\n Destination: {}\n\n",
                source_name, basic.name
            );
            content.push_str(" All configuration files (.nspawn) and systemd service\n overrides will be copied automatically.");
            return ContainerConfigWithPreview {
                cfg,
                preview: content,
                nvidia_profile: passthrough.nvidia_profile.clone(),
            };
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
            content.push_str("\n#[systemd override.conf]\n");
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
            nvidia_profile: passthrough.nvidia_profile.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get_deployer_and_storage(
        &self,
        system_operations: crate::nspawn::ops::SystemOperationStore,
        nspawn: crate::nspawn::adapters::config::NspawnConfigStore,
        systemd_unit: crate::nspawn::adapters::config::SystemdUnitStore,
        managed_storage: crate::nspawn::adapters::storage::ManagedStorageStore,
        bootstrap: crate::nspawn::ops::provision::BootstrapStore,
        image_import: crate::nspawn::ops::provision::ImageImportStore,
        oci_pull: crate::nspawn::ops::provision::OciPullStore,
    ) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        use crate::nspawn::adapters::storage::*;
        use crate::nspawn::ops::provision::*;

        let storage_cfg = self.storage.as_ref().cloned().unwrap_or(StorageConfig {
            storage_type: StorageType::Directory,
            disk_config: None,
        });

        let storage: Box<dyn StorageBackend> =
            match storage_cfg.storage_type {
                StorageType::Directory => Box::new(DirectoryBackend::new(managed_storage.clone()))
                    as Box<dyn StorageBackend>,
                StorageType::Subvolume => Box::new(SubvolumeBackend::new(managed_storage.clone()))
                    as Box<dyn StorageBackend>,
                StorageType::DiskImage => Box::new(DiskImageBackend::new(
                    storage_cfg
                        .disk_config
                        .unwrap_or(crate::nspawn::models::DiskImageConfig {
                            source: crate::nspawn::models::DiskImageSource::CreateNew {
                                size: "2G".to_string(),
                                fs_type: crate::nspawn::models::DiskImageFilesystem::Ext4,
                            },
                            use_partition_table: true,
                            root_partition: None,
                        }),
                    managed_storage,
                )) as Box<dyn StorageBackend>,
            };

        let source = self
            .source
            .as_ref()
            .cloned()
            .expect("deployment source must be configured");

        let deployer: Box<dyn Deployer> = match source {
            SourceConfig::Copy { source_name } => Box::new(clone::CloneDeployer {
                source_name,
                system_operations,
                nspawn,
                systemd_unit,
            }) as Box<dyn Deployer>,
            SourceConfig::Oci {
                reference,
                read_only,
            } => Box::new(oci::OciDeployer {
                reference,
                read_only,
                oci_pull,
            }) as Box<dyn Deployer>,
            SourceConfig::Artifact(artifact) => Box::new(image::ImageDeployer {
                source: image::ImageSource::Local(artifact.path.clone()),
                format: image::ImageFormat::from_artifact(&artifact),
                image_import: image_import.clone(),
            }) as Box<dyn Deployer>,
            SourceConfig::Pull { url, is_raw } => Box::new(image::ImageDeployer {
                source: image::ImageSource::Remote(url),
                format: if is_raw {
                    image::ImageFormat::Raw
                } else {
                    image::ImageFormat::Tar
                },
                image_import,
            }) as Box<dyn Deployer>,
            SourceConfig::Bootstrap(spec) => {
                Box::new(bootstrap::BootstrapDeployer { spec, bootstrap }) as Box<dyn Deployer>
            }
        };

        (deployer, storage)
    }
}

pub struct ContainerConfigWithPreview {
    pub cfg: ContainerConfig,
    pub preview: String,
    pub nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
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
    #[allow(clippy::field_reassign_with_default)]
    fn test_build_config_oci_disables_boot() {
        let mut builder = ContainerConfigBuilder::default();
        builder.source = Some(SourceConfig::Oci {
            reference: "docker.io/library/ubuntu".to_string(),
            read_only: false,
        });
        let result = builder.build_config(None);
        assert!(!result.cfg.boot);
        assert!(result.preview.contains("/var/lib/machines/unknown.mstack"));
        assert!(!result.preview.contains("[Exec]"));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_build_config_passthrough_fields() {
        let mut builder = ContainerConfigBuilder::default();
        builder.passthrough = Some(PassthroughConfig {
            bind_mounts: vec![],
            device_binds: vec!["/dev/dri/card0".to_string()],
            privileged: true,
            private_users: None,
            graphics_acceleration: true,
            wayland_socket: Some("wayland-0".to_string()),
            nvidia_gpu: true,
            nvidia_profile: None,
        });
        let result = builder.build_config(None);
        assert!(result.cfg.privileged);
        assert!(result.cfg.graphics_acceleration);
        assert_eq!(result.cfg.device_binds, vec!["/dev/dri/card0"]);
        assert_eq!(result.cfg.wayland_socket, Some("wayland-0".to_string()));
        assert!(result.cfg.nvidia_gpu);
    }
}
