use crate::application::provisioning::{
    DeploymentError, DeploymentRequest, DeploymentSource, DeploymentStorage, HostCapability,
    HostGpuDevice, HostHardwareSnapshot, ImagePartitionInfo, ImagePartitionProbe,
    InterfaceValidation, ProvisioningHostSnapshot, ProvisioningPreparationPort, StorageBackendKind,
    UnclassifiedNvidiaFile,
};
use crate::nspawn::models::DiskImageFilesystem;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct NspawnProvisioningPreparation;

#[async_trait]
impl ProvisioningPreparationPort for NspawnProvisioningPreparation {
    async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
        let storage = crate::adapters::storage::detect::detect_available_storage_types().await;
        let storage_backends = storage
            .types
            .into_iter()
            .map(|(kind, available)| {
                let kind = match kind {
                    crate::adapters::storage::StorageType::Directory => {
                        StorageBackendKind::Directory
                    }
                    crate::adapters::storage::StorageType::Subvolume => {
                        StorageBackendKind::Subvolume
                    }
                    crate::adapters::storage::StorageType::DiskImage => {
                        StorageBackendKind::DiskImage
                    }
                };
                (kind, available)
            })
            .collect();

        let mut tools = BTreeMap::new();
        for tool in [
            "debootstrap",
            "pacstrap",
            "dnf5",
            "curl",
            "sfdisk",
            "losetup",
            "udevadm",
        ] {
            tools.insert(tool.to_string(), tool_available(tool));
        }
        let filesystems = DiskImageFilesystem::ALL
            .iter()
            .copied()
            .map(|filesystem| {
                let available = tool_available(filesystem.mkfs_tool());
                tools.insert(filesystem.mkfs_tool().to_string(), available);
                (filesystem, available)
            })
            .collect();

        let oci =
            match crate::adapters::provisioning::engine::oci_operation::ensure_pull_oci_available()
            {
                Ok(()) => HostCapability::available(),
                Err(error) => HostCapability::unavailable(error.to_string()),
            };

        Ok(ProvisioningHostSnapshot {
            storage_backends,
            tools,
            filesystems,
            oci,
            bridges: crate::adapters::platform::network::detect_bridges().await,
            physical_interfaces: crate::adapters::platform::network::detect_physical_interfaces()
                .await,
            wayland_sockets: crate::adapters::platform::capabilities::discover_wayland_sockets()
                .await,
            nvidia_toolkit_installed: crate::adapters::platform::nvidia::nvidia_ctk_available(),
        })
    }

    async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
        let gpus = crate::adapters::platform::gpu::discover_host_gpus()
            .await
            .into_iter()
            .map(|gpu| HostGpuDevice {
                display_name: gpu.display_name,
                driver_type: gpu.driver_type,
                nodes: gpu.nodes,
            })
            .collect();
        let mut nvidia_devices = Vec::new();
        let mut active_nvidia_categories = Vec::new();
        let mut unclassified_nvidia_files = Vec::new();
        let mut warnings = Vec::new();

        if crate::adapters::platform::nvidia::nvidia_ctk_available() {
            match crate::adapters::platform::nvidia::discovery::discover_hardware().await {
                Ok((devices, state)) => {
                    nvidia_devices = devices;
                    active_nvidia_categories =
                        crate::adapters::platform::nvidia::classify::detect_active_categories(
                            &state.classified_entries,
                        );
                    unclassified_nvidia_files = state
                        .binds
                        .into_iter()
                        .filter(|bind| {
                            bind.readonly
                                && crate::adapters::platform::nvidia::classify::classify_path(
                                    &bind.container_path,
                                )
                                .is_none()
                        })
                        .map(|bind| UnclassifiedNvidiaFile {
                            host_path: bind.host_path,
                            default_container_path: bind.container_path,
                            readonly: true,
                        })
                        .collect();
                }
                Err(error) => warnings.push(format!("NVIDIA CDI discovery failed: {error}")),
            }
        }

        Ok(HostHardwareSnapshot {
            nvidia_devices,
            gpus,
            active_nvidia_categories,
            unclassified_nvidia_files,
            warnings,
        })
    }

    async fn validate_interface(
        &self,
        name: &str,
        bridge_mode: bool,
    ) -> Result<InterfaceValidation, DeploymentError> {
        let net_path = format!("/sys/class/net/{name}");
        let bridge_path = format!("/sys/class/net/{name}/bridge");
        let exists = tokio::fs::metadata(&net_path).await.is_ok();
        let is_bridge = tokio::fs::metadata(&bridge_path).await.is_ok();
        let warning = if !exists {
            Some(format!(
                "Interface '{name}' not found. It must exist before starting the container."
            ))
        } else if bridge_mode && !is_bridge {
            let actual = crate::adapters::platform::network::identify_interface(name).await;
            Some(format!("'{name}' is a {actual}, not a bridge"))
        } else if !bridge_mode && is_bridge {
            Some(format!(
                "'{name}' is a bridge, but you selected a physical/virtual mode"
            ))
        } else {
            None
        };
        Ok(warning.map_or(InterfaceValidation::Valid, InterfaceValidation::Warning))
    }

    fn probe_image(&self, path: &Path) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
        let probe = crate::adapters::storage::image_ops::probe_image_partitions(path)
            .map_err(|error| DeploymentError::failed(error.to_string()))?;
        probe
            .map(|probe| {
                let partitions = probe
                    .partitions
                    .into_iter()
                    .map(|partition| {
                        let current_architecture_root =
                            crate::adapters::storage::image_ops::is_current_architecture_root_type(
                                &partition.type_id,
                            )
                            .map_err(|error| DeploymentError::failed(error.to_string()))?;
                        Ok(ImagePartitionInfo {
                            number: partition.number,
                            type_label: crate::adapters::storage::image_ops::partition_type_label(
                                &partition.type_id,
                            ),
                            current_architecture_root,
                        })
                    })
                    .collect::<Result<Vec<_>, DeploymentError>>()?;
                Ok(ImagePartitionProbe {
                    label: probe.label,
                    partitions,
                })
            })
            .transpose()
    }

    fn preview(&self, request: &DeploymentRequest) -> String {
        if let DeploymentSource::Oci {
            reference,
            read_only,
            network,
        } = &request.source
        {
            let mode = if *read_only {
                "read-only layers"
            } else {
                "writable overlay"
            };
            return format!(
                " [SYSTEMD OCI APPLICATION]\n\n Reference: {reference}\n Name: {}\n Storage: /var/lib/machines/{}.mstack\n Mode: {mode}\n Network: {}\n PrivateUsers: no (system-scope import)\n Runtime config: OCI settings preserved in trusted host config\n Verification: HTTPS transport authentication; no publisher signature verification\n",
                request.config.name,
                request.config.name,
                network.as_str(),
            );
        }
        if let DeploymentSource::Copy { source_name } = &request.source {
            return format!(
                " [CLONE OPERATION]\n\n Source: {source_name}\n Destination: {}\n\n All configuration files (.nspawn) and systemd service\n overrides will be copied automatically.",
                request.config.name
            );
        }

        let (storage_label, storage_path) = match &request.storage {
            DeploymentStorage::Directory => (
                "Directory",
                crate::paths::machine_root(&request.config.name),
            ),
            DeploymentStorage::Subvolume => (
                "Btrfs Subvolume",
                crate::paths::machine_root(&request.config.name),
            ),
            DeploymentStorage::DiskImage(_) => (
                "Disk Image (Raw/Block)",
                crate::paths::machine_raw_image(&request.config.name),
            ),
        };
        let mut content = format!(
            " [DEPLOYMENT PREVIEW - {}]\n\n Storage: {storage_label} ({})\n Hostname: {}\n",
            request.config.name,
            storage_path.display(),
            request.config.hostname,
        );
        if let DeploymentSource::Bootstrap(spec) = &request.source {
            if spec.inherits_default_packages() {
                content.push_str(" Bootstrap packages: defaults + configured additions\n");
            } else {
                content.push_str(" WARNING: Default packages are disabled; configured packages must satisfy the container runtime and network requirements.\n");
            }
        }
        for intent in &request.wayland {
            let displays = intent
                .sources()
                .iter()
                .map(|source| source.display().as_str())
                .collect::<Vec<_>>()
                .join(", ");
            content.push_str(&format!(
                " Wayland access: {displays} -> {} (default {})\n",
                intent.target_username(),
                intent.default_display().as_str(),
            ));
        }
        let rendered = (|| -> Result<String, String> {
            request.validate().map_err(|error| error.to_string())?;
            let spec = crate::nspawn::models::NspawnConfigSpec::try_from(&request.config)
                .map_err(|error| error.to_string())?;
            let wayland_binds = if request.wayland.is_empty() {
                Vec::new()
            } else {
                let policy = crate::application::provisioning::resolve_wayland_bind_policy(
                    request.config.private_users,
                )
                .map_err(|error| error.to_string())?;
                request
                    .wayland
                    .iter()
                    .flat_map(|intent| {
                        intent.sources().iter().map(move |source| {
                            crate::adapters::wayland::WaylandBind::new(
                                source.canonical_path(),
                                intent.required_uid(),
                                source.display(),
                                policy,
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            };
            crate::adapters::config::nspawn_file::nspawn_config_content_from_spec_with_wayland_binds(
                &spec,
                &wayland_binds,
            )
            .map_err(|error| error.to_string())
        })();
        content.push_str(&rendered.unwrap_or_else(|error| format!(" [ERROR: {error}]")));
        if !request.config.device_binds.is_empty() || request.config.gpu_passthrough_all {
            content.push_str("\n#[systemd override.conf]\n");
            content.push_str(
                &crate::adapters::config::systemd_unit::systemd_override_content(
                    &request.config.device_binds,
                    request.config.gpu_passthrough_all,
                ),
            );
        }
        content
    }
}

fn tool_available(name: &str) -> bool {
    which::which(name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{
        HostWaylandSocket, SocketRevision, WaylandDisplay, WaylandGrantIntent,
    };
    use crate::nspawn::models::{ContainerConfig, CreateUser, NetworkMode, PrivateUsersMode};

    fn request(private_users: PrivateUsersMode) -> DeploymentRequest {
        let source = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1001".into(),
            "/run/user/1001/wayland-0".into(),
            1001,
            1001,
            1001,
            0o755,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        DeploymentRequest {
            config: ContainerConfig {
                name: "test".into(),
                private_users: Some(private_users),
                network: Some(NetworkMode::Veth),
                users: vec![CreateUser {
                    username: "lumie".into(),
                    uid: Some(1001),
                    shell: "/bin/bash".into(),
                    sudoer: false,
                }],
                ..Default::default()
            },
            source: DeploymentSource::Pull {
                url: "https://example.test/rootfs.raw".into(),
                is_raw: true,
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: vec![WaylandGrantIntent::new(
                "lumie",
                vec![source.clone()],
                source.display().clone(),
            )
            .unwrap()],
            allow_unsafe_remote_tar: false,
        }
    }

    #[test]
    fn preview_uses_the_same_wayland_endpoint_and_bind_policy_as_apply() {
        let adapter = NspawnProvisioningPreparation;
        let idmapped = adapter.preview(&request(PrivateUsersMode::Pick));
        assert!(idmapped.contains("Wayland access: wayland-0 -> lumie (default wayland-0)"));
        assert!(idmapped
            .contains("Bind=/run/user/1001/wayland-0:/run/lasper/wayland/1001/wayland-0:idmap"));

        let direct = adapter.preview(&request(PrivateUsersMode::No));
        assert!(direct
            .contains("Bind=/run/user/1001/wayland-0:/run/lasper/wayland/1001/wayland-0:noidmap"));
    }

    #[test]
    fn preview_reports_unsupported_managed_wayland_policy() {
        let preview = NspawnProvisioningPreparation.preview(&request(PrivateUsersMode::Managed));
        assert!(preview.contains("[ERROR:"));
        assert!(preview.contains("not supported with PrivateUsers=managed"));
        assert!(!preview.contains("/run/lasper/wayland/1001/wayland-0:idmap"));
    }
}
