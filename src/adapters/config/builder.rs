use crate::adapters::config::nspawn_file::nspawn_config_content;
use crate::adapters::config::systemd_unit::systemd_override_content;
use crate::adapters::storage::StorageType;
use crate::nspawn::models::{ArtifactSpec, BootstrapMethod, BootstrapSpec};
use crate::nspawn::models::{
    BindMount, ContainerConfig, CreateUser, NetworkMode, OciNetworkMode, PortForward,
    PrivateUsersMode,
};

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
    Copy {
        source_name: String,
    },
    Oci {
        reference: String,
        read_only: bool,
        network: OciNetworkMode,
    },
    Bootstrap(BootstrapSpec),
    Pull {
        url: String,
        is_raw: bool,
    },
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
    pub private_users: Option<PrivateUsersMode>,
    pub graphics_acceleration: bool,
    pub gpu_passthrough_all: bool,
    pub wayland_socket: Option<String>,
    pub nvidia_gpu: bool,
    pub nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
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
                gpu_passthrough_all: false,
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

        let user = self
            .user
            .as_ref()
            .cloned()
            .unwrap_or(UserConfig { users: vec![] });

        let device_binds = passthrough.device_binds.clone();

        let (network, private_users) = match &self.source {
            Some(SourceConfig::Oci { network, .. }) => (
                Some(network.into_network_mode()),
                Some(PrivateUsersMode::No),
            ),
            _ => (nw.mode.clone(), passthrough.private_users),
        };

        let cfg = ContainerConfig {
            name: basic.name.clone(),
            hostname: basic.hostname.clone(),
            network,
            port_forwards: nw.port_forwards.clone(),
            bind_mounts: passthrough.bind_mounts.clone(),
            device_binds,
            readonly_binds: vec![],
            privileged: passthrough.privileged,
            private_users,
            graphics_acceleration: passthrough.graphics_acceleration,
            gpu_passthrough_all: passthrough.gpu_passthrough_all,
            users: user.users.clone(),
            wayland_socket: passthrough.wayland_socket.clone(),
            nvidia_gpu: passthrough.nvidia_gpu,
            disk_config: storage.disk_config.clone(),
            boot: !matches!(self.source, Some(SourceConfig::Oci { .. })),
        };

        if let Some(SourceConfig::Oci {
            reference,
            read_only,
            network,
        }) = &self.source
        {
            let mode = if *read_only {
                "read-only layers"
            } else {
                "writable overlay"
            };
            let preview = format!(
                " [SYSTEMD OCI APPLICATION]\n\n Reference: {reference}\n Name: {}\n Storage: /var/lib/machines/{}.mstack\n Mode: {mode}\n Network: {}\n PrivateUsers: no (system-scope import)\n Runtime config: OCI settings preserved in trusted host config\n Verification: HTTPS transport authentication; no publisher signature verification\n",
                basic.name,
                basic.name,
                network.as_str(),
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
        if let Some(SourceConfig::Bootstrap(spec)) = &self.source {
            if spec.inherits_default_packages() {
                content.push_str(" Bootstrap packages: defaults + configured additions\n");
            } else {
                content.push_str(
                    " WARNING: Default packages are disabled; configured packages must satisfy the container runtime and network requirements.\n",
                );
            }
        }
        content.push_str(
            &nspawn_config_content(&cfg, xdg_runtime)
                .unwrap_or_else(|e| format!(" [ERROR: {}]", e)),
        );
        if !cfg.device_binds.is_empty() || cfg.gpu_passthrough_all {
            content.push_str("\n#[systemd override.conf]\n");
            content.push_str(&systemd_override_content(
                &cfg.device_binds,
                cfg.gpu_passthrough_all,
            ));
        }

        ContainerConfigWithPreview {
            cfg,
            preview: content,
            nvidia_profile: passthrough.nvidia_profile.clone(),
        }
    }
}

pub struct ContainerConfigWithPreview {
    pub cfg: ContainerConfig,
    pub preview: String,
    pub nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
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
            network: OciNetworkMode::Host,
        });
        let result = builder.build_config(None);
        assert!(!result.cfg.boot);
        assert!(result.preview.contains("/var/lib/machines/unknown.mstack"));
        assert!(result.preview.contains("Network: host"));
        assert!(result.preview.contains("PrivateUsers: no"));
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
            gpu_passthrough_all: false,
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
