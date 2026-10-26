use crate::application::provisioning::{
    DeploymentError, HostCapability, HostGpuDevice, HostHardwareSnapshot, ImagePartitionInfo,
    ImagePartitionProbe, InterfaceValidation, ProvisioningHostSnapshot,
    ProvisioningPreparationPort, StorageBackendKind, UnclassifiedNvidiaFile,
};
use crate::domain::provisioning::source::BootstrapMethod;
use crate::domain::provisioning::storage::DiskImageFilesystem;
use crate::domain::provisioning::ImageImport;
use crate::domain::provisioning::{DeploymentSpec, DeploymentStorage};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct NspawnProvisioningPreparation {
    nvidia_cdi_source: crate::domain::nvidia::NvidiaCdiSource,
}

impl NspawnProvisioningPreparation {
    pub(crate) fn new(nvidia_cdi_source: crate::domain::nvidia::NvidiaCdiSource) -> Self {
        Self { nvidia_cdi_source }
    }
}

fn filesystem_tool(filesystem: DiskImageFilesystem) -> &'static str {
    match filesystem {
        DiskImageFilesystem::Ext4 => "mkfs.ext4",
        DiskImageFilesystem::Xfs => "mkfs.xfs",
        DiskImageFilesystem::Btrfs => "mkfs.btrfs",
    }
}

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
        for tool in BootstrapMethod::ALL
            .into_iter()
            .flat_map(BootstrapMethod::required_tools)
            .copied()
            .chain(["curl", "sfdisk", "losetup", "udevadm"])
        {
            tools.insert(tool.to_string(), tool_available(tool));
        }
        let filesystems = DiskImageFilesystem::ALL
            .iter()
            .copied()
            .map(|filesystem| {
                let tool = filesystem_tool(filesystem);
                let available = tool_available(tool);
                tools.insert(tool.to_string(), available);
                (filesystem, available)
            })
            .collect();

        let oci =
            match crate::adapters::provisioning::engine::oci_operation::ensure_pull_oci_available()
            {
                Ok(()) => HostCapability::available(),
                Err(error) => HostCapability::unavailable(error.to_string()),
            };

        let x11 = crate::adapters::platform::x11::discover_host_x11_sockets().await;
        let x11_socket_directory_available =
            crate::adapters::platform::x11::x11_socket_directory_available().await;
        let wayland = crate::adapters::platform::wayland::discover_wayland_socket_catalog().await;

        Ok(ProvisioningHostSnapshot {
            storage_backends,
            tools,
            filesystems,
            oci,
            bridges: crate::adapters::platform::network::detect_bridges().await,
            physical_interfaces: crate::adapters::platform::network::detect_physical_interfaces()
                .await,
            wayland_sockets: wayland.sockets,
            preferred_wayland_display: wayland.preferred_display,
            x11_sockets: x11.sockets,
            preferred_x11_display: x11.preferred_display,
            x11_socket_directory_available,
            nvidia_cdi_available:
                crate::adapters::platform::nvidia::discovery::cdi_source_available(
                    &self.nvidia_cdi_source,
                )
                .await,
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
        let cdi_available = crate::adapters::platform::nvidia::discovery::cdi_source_available(
            &self.nvidia_cdi_source,
        )
        .await;
        let mut nvidia_devices = Vec::new();
        let mut active_nvidia_categories = Vec::new();
        let mut unclassified_nvidia_files = Vec::new();
        let mut warnings = Vec::new();

        if !cdi_available {
            warnings.push(format!(
                "{} is unavailable",
                self.nvidia_cdi_source.description()
            ));
        } else {
            match crate::adapters::platform::nvidia::discovery::discover_hardware_from(
                &self.nvidia_cdi_source,
            )
            .await
            {
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

    fn preview(&self, request: &DeploymentSpec) -> String {
        let (machine, storage, wayland, provider) = match request {
            DeploymentSpec::Oci(deployment) => {
                let mode = if deployment.read_only { "read-only layers" } else { "writable overlay" };
                return format!(
                    " [SYSTEMD OCI APPLICATION]\n\n Reference: {}\n Name: {}\n Storage: /var/lib/machines/{}.mstack\n Mode: {mode}\n Network: {}\n PrivateUsers: no (system-scope import)\n Runtime config: OCI settings preserved in trusted host config\n Verification: HTTPS transport authentication; no publisher signature verification\n",
                    deployment.reference, deployment.name, deployment.name, deployment.network.as_str(),
                );
            }
            DeploymentSpec::Clone(deployment) => return format!(
                " [CLONE OPERATION]\n\n Source: {}\n Destination: {}\n\n Image data, effective .nspawn settings, and Lasper's service override\n are copied when present.\n\n WARNING: {}",
                deployment.source_name, deployment.name,
                crate::adapters::provisioning::engine::builders::clone::CLONE_IDENTITY_NOTICE,
            ),
            DeploymentSpec::Bootstrap(deployment) => (&deployment.machine, Some(&deployment.storage), deployment.wayland.as_slice(), Some(&deployment.provider)),
            DeploymentSpec::Import(deployment) => {
                let storage = match &deployment.image {
                    ImageImport::Archive { storage, .. } => Some(storage),
                    ImageImport::DiskImage { .. } => None,
                };
                (&deployment.machine, storage, &[][..], None)
            }
        };
        let (storage_label, storage_path) = match storage {
            Some(DeploymentStorage::Directory) => {
                ("Directory", crate::paths::machine_root(&machine.name))
            }
            Some(DeploymentStorage::Subvolume) => {
                ("Btrfs Subvolume", crate::paths::machine_root(&machine.name))
            }
            Some(DeploymentStorage::DiskImage(_)) | None => (
                "Disk Image (Raw/Block)",
                crate::paths::machine_raw_image(&machine.name),
            ),
        };
        let guest_hostname = crate::adapters::config::NspawnConfigSpec::try_from(machine)
            .map(|spec| spec.guest_hostname.into_string())
            .unwrap_or_else(|_| machine.guest_hostname.clone());
        let mut content =
            format!(
            " [DEPLOYMENT PREVIEW - {}]\n\n Storage: {storage_label} ({})\n Guest hostname: {}\n",
            machine.name, storage_path.display(), guest_hostname,
        );
        if let Some(provider) = provider {
            if provider.inherits_default_packages() {
                content.push_str(" Bootstrap packages: defaults + configured additions\n");
            } else {
                content.push_str(" WARNING: Default packages are disabled; configured packages must satisfy the container runtime and network requirements.\n");
            }
        } else {
            content.push_str(" Accounts: existing users and credentials are preserved\n");
        }
        for intent in wayland {
            let displays = intent
                .sources()
                .iter()
                .map(|source| source.display().as_str())
                .collect::<Vec<_>>()
                .join(", ");
            content.push_str(&format!(
                " Wayland access: {displays} -> {}\n",
                intent.target_username()
            ));
        }
        let rendered = (|| -> Result<String, String> {
            request.validate().map_err(|error| error.to_string())?;
            let spec = crate::adapters::config::NspawnConfigSpec::try_from(machine)
                .map_err(|error| error.to_string())?;
            let wayland_binds = if wayland.is_empty() {
                Vec::new()
            } else {
                let policy = crate::domain::provisioning::wayland::resolve_wayland_bind_policy(
                    machine.private_users,
                )
                .map_err(|error| error.to_string())?;
                wayland
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
            crate::adapters::config::nspawn_file::nspawn_config_content_from_spec_with_wayland_binds(&spec, &wayland_binds).map_err(|error| error.to_string())
        })();
        content.push_str(&rendered.unwrap_or_else(|error| format!(" [ERROR: {error}]")));
        if !machine.device_binds.is_empty() || machine.gpu_passthrough_all {
            content.push_str("\n#[systemd override.conf]\n");
            content.push_str(
                &crate::adapters::config::systemd_unit::systemd_override_content(
                    &machine.device_binds,
                    machine.gpu_passthrough_all,
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
    use crate::domain::machine::CreateUser;
    use crate::domain::machine::MachineConfig;
    use crate::domain::machine::{NetworkMode, PrivateUsersMode};
    use crate::domain::wayland::{
        HostWaylandSocket, SocketRevision, WaylandDisplay, WaylandGrantIntent,
    };

    fn request(private_users: PrivateUsersMode) -> DeploymentSpec {
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
        DeploymentSpec::Bootstrap(Box::new(crate::domain::provisioning::BootstrapDeployment {
            machine: MachineConfig {
                name: "test".into(),
                private_users: Some(private_users),
                network: Some(NetworkMode::Veth),
                ..Default::default()
            },
            provider: crate::domain::provisioning::bootstrap::BootstrapSpec::Pacstrap(
                Default::default(),
            ),
            users: vec![CreateUser {
                username: "lumie".into(),
                uid: Some(1001),
                shell: "/bin/bash".into(),
                sudoer: false,
            }],
            storage: DeploymentStorage::Directory,
            integration: Default::default(),
            wayland: vec![WaylandGrantIntent::new("lumie", vec![source]).unwrap()],
        }))
    }

    #[test]
    fn preview_uses_the_same_wayland_endpoint_and_bind_policy_as_apply() {
        let adapter = NspawnProvisioningPreparation::new(Default::default());
        let idmapped = adapter.preview(&request(PrivateUsersMode::Pick));
        assert!(idmapped.contains("Wayland access: wayland-0 -> lumie"));
        assert!(idmapped
            .contains("Bind=/run/user/1001/wayland-0:/run/lasper/wayland/1001/wayland-0:idmap"));

        let direct = adapter.preview(&request(PrivateUsersMode::No));
        assert!(direct
            .contains("Bind=/run/user/1001/wayland-0:/run/lasper/wayland/1001/wayland-0:noidmap"));
    }

    #[test]
    fn preview_reports_unsupported_managed_wayland_policy() {
        let preview = NspawnProvisioningPreparation::new(Default::default())
            .preview(&request(PrivateUsersMode::Managed));
        assert!(preview.contains("[ERROR:"));
        assert!(preview.contains("not supported with PrivateUsers=managed"));
        assert!(!preview.contains("/run/lasper/wayland/1001/wayland-0:idmap"));
    }

    #[test]
    fn clone_preview_discloses_preserved_guest_identity() {
        let request = DeploymentSpec::Clone(crate::domain::provisioning::CloneDeployment {
            name: "test".into(),
            source_name: "source-machine".into(),
        });

        let preview = NspawnProvisioningPreparation::new(Default::default()).preview(&request);

        assert!(preview.contains("Exact clone"));
        assert!(preview.contains("machine-id"));
        assert!(preview.contains("SSH host keys"));
        assert!(preview.contains("Reset guest identity manually"));
    }
}
