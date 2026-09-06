use crate::application::provisioning::{
    DeploymentRequest, DeploymentSecrets, DeploymentSource, DeploymentStorage,
    DeploymentSubmission, HostGpuDevice, HostHardwareSnapshot, ProvisioningHostSnapshot,
    StorageBackendKind, UserSecret,
};
use crate::domain::bootstrap::{
    BootstrapSpec, Dnf5RepositorySource, RootfsSourceSpec, DEFAULT_BOOTSTRAP_PROFILE,
};
use crate::domain::provisioning::{
    BindMount, CreateUser, NetworkMode, OciNetworkMode, PortForward, PrivateUsersMode,
};
use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::domain::secret::zeroize_string;
use crate::domain::source::{ArtifactSpec, BootstrapMethod};
use crate::domain::storage::{
    DiskImageConfig, DiskImageFilesystem, DiskImagePartition, DiskImageSource,
};
use crate::domain::wayland::{HostWaylandSocket, WaylandGrantIntent, WaylandValidationError};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub guest_hostname: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserConfig {
    pub users: Vec<CreateUser>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub storage_type: StorageBackendKind,
    pub disk_config: Option<DiskImageConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageInfo {
    pub types: Vec<(StorageBackendKind, bool)>,
    pub filesystems: Vec<(DiskImageFilesystem, bool)>,
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
    pub nvidia_gpu: bool,
    pub nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredSourceProfile {
    pub method: BootstrapMethod,
    pub name: String,
    pub source: RootfsSourceSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub kind: SourceKind,
    pub oci_url: String,
    pub oci_read_only: bool,
    pub oci_network: OciNetworkMode,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub deboot_pkgs: String,
    pub deboot_inherit_default_packages: bool,
    pub pacstrap_pkgs: String,
    pub pacstrap_inherit_default_packages: bool,
    pub dnf_releasever: String,
    pub dnf_pkgs: String,
    pub dnf_inherit_default_packages: bool,
    pub local_path: String,
    pub clone_source: String,
    pub pull_url: String,
    pub is_pull_raw: bool,
    unsafe_tar_accepted_for: Option<String>,
    pub copy_idx: usize,
    pub profiles: Vec<ConfiguredSourceProfile>,
    pub default_profiles: Vec<ConfiguredSourceProfile>,
}

impl SourceState {
    pub fn remote_tar_url(&self) -> Option<&str> {
        (self.kind == SourceKind::Pull && !self.is_pull_raw)
            .then(|| self.pull_url.trim())
            .filter(|url| !url.is_empty())
    }

    pub fn unsafe_remote_tar_accepted(&self) -> bool {
        self.remote_tar_url()
            .is_some_and(|url| self.unsafe_tar_accepted_for.as_deref() == Some(url))
    }

    pub fn accept_unsafe_remote_tar(&mut self) -> bool {
        let Some(url) = self.remote_tar_url().map(str::to_owned) else {
            return false;
        };
        self.unsafe_tar_accepted_for = Some(url);
        true
    }

    pub fn extract_config(&self) -> SourceConfig {
        match &self.kind {
            SourceKind::Copy => SourceConfig::Copy {
                source_name: self.clone_source.clone(),
            },
            SourceKind::Oci => SourceConfig::Oci {
                reference: self.oci_url.trim().into(),
                read_only: self.oci_read_only,
                network: self.oci_network,
            },
            SourceKind::Debootstrap => {
                let mut spec = self
                    .default_source(BootstrapMethod::Debootstrap)
                    .and_then(|source| match source {
                        RootfsSourceSpec::Debootstrap(spec) => Some(spec.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                spec.suite = self.deboot_suite.trim().into();
                spec.mirror = nonempty(&self.deboot_mirror);
                spec.packages = split_packages(&self.deboot_pkgs);
                spec.inherit_default_packages = self.deboot_inherit_default_packages;
                SourceConfig::Bootstrap(BootstrapSpec::Debootstrap(spec))
            }
            SourceKind::Pacstrap => {
                let mut spec = self
                    .default_source(BootstrapMethod::Pacstrap)
                    .and_then(|source| match source {
                        RootfsSourceSpec::Pacstrap(spec) => Some(spec.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                spec.packages = split_packages(&self.pacstrap_pkgs);
                spec.inherit_default_packages = self.pacstrap_inherit_default_packages;
                SourceConfig::Bootstrap(BootstrapSpec::Pacstrap(spec))
            }
            SourceKind::Dnf5 => {
                let mut spec = self
                    .default_source(BootstrapMethod::Dnf5)
                    .and_then(|source| match source {
                        RootfsSourceSpec::Dnf5(spec) => Some(spec.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                spec.releasever = self.dnf_releasever.trim().into();
                spec.packages = split_packages(&self.dnf_pkgs);
                spec.inherit_default_packages = self.dnf_inherit_default_packages;
                if spec.repository == Dnf5RepositorySource::Unspecified {
                    spec.repository = Dnf5RepositorySource::Host;
                }
                SourceConfig::Bootstrap(BootstrapSpec::Dnf5(spec))
            }
            SourceKind::Pull => SourceConfig::Pull {
                url: self.pull_url.clone(),
                is_raw: self.is_pull_raw,
            },
            SourceKind::LocalFile => SourceConfig::Artifact(self.artifact_spec()),
            SourceKind::Profile { method, name } => self
                .profiles
                .iter()
                .find(|profile| &profile.method == method && &profile.name == name)
                .map(|profile| source_config_from_profile(&profile.source))
                .unwrap_or_else(|| SourceConfig::Artifact(ArtifactSpec::from_path(""))),
        }
    }

    pub fn is_storage_managed_externally(&self) -> bool {
        match &self.kind {
            SourceKind::Oci => true,
            SourceKind::Pull => self.is_pull_raw,
            SourceKind::LocalFile => self.artifact_spec().is_external_storage(),
            SourceKind::Profile { method, name } => self
                .profiles
                .iter()
                .find(|profile| &profile.method == method && &profile.name == name)
                .is_some_and(|profile| profile.source.is_external_storage()),
            _ => false,
        }
    }

    fn default_source(&self, method: BootstrapMethod) -> Option<&RootfsSourceSpec> {
        self.default_profiles
            .iter()
            .find(|profile| profile.method == method)
            .map(|profile| &profile.source)
    }

    fn artifact_spec(&self) -> ArtifactSpec {
        let configured = self
            .default_source(BootstrapMethod::Artifact)
            .and_then(|source| match source {
                RootfsSourceSpec::Artifact(spec) => Some(spec),
                _ => None,
            });
        match configured {
            Some(spec) if spec.expanded_path() == self.local_path => {
                let mut spec = spec.clone();
                spec.path = self.local_path.clone();
                spec
            }
            _ => ArtifactSpec::from_path(self.local_path.clone()),
        }
    }
}

fn split_packages(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_string).collect()
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn source_config_from_profile(source: &RootfsSourceSpec) -> SourceConfig {
    match source {
        RootfsSourceSpec::Debootstrap(spec) => {
            SourceConfig::Bootstrap(BootstrapSpec::Debootstrap(spec.clone()))
        }
        RootfsSourceSpec::Pacstrap(spec) => {
            SourceConfig::Bootstrap(BootstrapSpec::Pacstrap(spec.clone()))
        }
        RootfsSourceSpec::Dnf5(spec) => SourceConfig::Bootstrap(BootstrapSpec::Dnf5(spec.clone())),
        RootfsSourceSpec::Artifact(spec) => SourceConfig::Artifact(ArtifactSpec {
            path: spec.expanded_path(),
            format: spec.format,
        }),
    }
}

fn source_kind_for_method(method: BootstrapMethod) -> SourceKind {
    match method {
        BootstrapMethod::Debootstrap => SourceKind::Debootstrap,
        BootstrapMethod::Pacstrap => SourceKind::Pacstrap,
        BootstrapMethod::Dnf5 => SourceKind::Dnf5,
        BootstrapMethod::Artifact => SourceKind::LocalFile,
    }
}

fn configured_default_source_kind(
    profiles: &[ConfiguredSourceProfile],
    default_method: Option<BootstrapMethod>,
    default_profile: Option<String>,
) -> SourceKind {
    let Some(method) = default_method else {
        return SourceKind::Copy;
    };
    let Some(name) = default_profile else {
        return source_kind_for_method(method);
    };
    if profiles
        .iter()
        .any(|profile| profile.method == method && profile.name == name)
    {
        return SourceKind::Profile { method, name };
    }
    if name != DEFAULT_BOOTSTRAP_PROFILE {
        log::warn!(
            "Bootstrap default profile '{}' is missing or invalid for {:?}; using the built-in method",
            name,
            method
        );
    }
    source_kind_for_method(method)
}

fn method_default(
    defaults: &[ConfiguredSourceProfile],
    method: BootstrapMethod,
) -> Option<&RootfsSourceSpec> {
    defaults
        .iter()
        .find(|profile| profile.method == method)
        .map(|profile| &profile.source)
}

#[derive(Debug, Clone, PartialEq)]
pub struct BasicState {
    pub name: String,
    pub guest_hostname: String,
}

impl BasicState {
    pub fn extract_config(&self) -> BasicConfig {
        BasicConfig {
            name: self.name.clone(),
            guest_hostname: self.guest_hostname.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageState {
    pub type_idx: usize,
    pub info: StorageInfo,
    pub creation_method_idx: usize, // 0: Create New, 1: Import Existing
    pub disk_size: String,
    pub disk_fs: DiskImageFilesystem,
    pub disk_partition: bool,
    pub import_path: String,
    pub disk_root_partition: Option<DiskImagePartition>,
}

impl StorageState {
    pub fn extract_config(&self) -> StorageConfig {
        let (st, _) = self.info.types[self.type_idx];
        StorageConfig {
            storage_type: st,
            disk_config: if st == StorageBackendKind::DiskImage {
                let source = if self.creation_method_idx == 1 {
                    DiskImageSource::ImportExisting {
                        path: self.import_path.clone(),
                    }
                } else {
                    DiskImageSource::CreateNew {
                        size: self.disk_size.clone(),
                        fs_type: self.disk_fs,
                    }
                };

                Some(DiskImageConfig {
                    source,
                    use_partition_table: self.disk_partition,
                    root_partition: if self.creation_method_idx == 1 {
                        self.disk_root_partition
                    } else {
                        None
                    },
                })
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandAccessDraft {
    pub sockets: Vec<HostWaylandSocket>,
}

impl WaylandAccessDraft {
    pub fn new(sockets: Vec<HostWaylandSocket>) -> Result<Self, WaylandValidationError> {
        WaylandGrantIntent::new("draft-user", sockets.clone())?;
        Ok(Self { sockets })
    }

    pub fn intent_for(&self, username: &str) -> WaylandGrantIntent {
        WaylandGrantIntent::new(username, self.sockets.clone())
            .expect("Wayland draft invariants were checked when it was created")
    }

    pub fn required_uid(&self) -> u32 {
        self.sockets[0].owner_uid()
    }
}

#[derive(PartialEq)]
pub struct UserDraft {
    pub username: String,
    pub password: String,
    pub sudoer: bool,
    pub shell: String,
    pub wayland: Option<WaylandAccessDraft>,
}

impl UserDraft {
    pub fn account(&self) -> CreateUser {
        CreateUser {
            username: self.username.clone(),
            uid: self.wayland.as_ref().map(WaylandAccessDraft::required_uid),
            sudoer: self.sudoer,
            shell: self.shell.clone(),
        }
    }

    pub fn editing_copy(&self) -> Self {
        Self {
            username: self.username.clone(),
            password: self.password.clone(),
            sudoer: self.sudoer,
            shell: self.shell.clone(),
            wayland: self.wayland.clone(),
        }
    }
}

impl std::fmt::Debug for UserDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserDraft")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("sudoer", &self.sudoer)
            .field("shell", &self.shell)
            .field("wayland", &self.wayland)
            .finish()
    }
}

impl Drop for UserDraft {
    fn drop(&mut self) {
        zeroize_string(&mut self.password);
    }
}

pub struct UserState {
    pub root_password: String,
    pub users: Vec<UserDraft>,
}

impl std::fmt::Debug for UserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserState")
            .field("root_password", &"[REDACTED]")
            .field("users", &self.users)
            .finish()
    }
}

impl Drop for UserState {
    fn drop(&mut self) {
        zeroize_string(&mut self.root_password);
    }
}

impl UserState {
    pub fn extract_config(&self) -> UserConfig {
        UserConfig {
            users: self.users.iter().map(UserDraft::account).collect(),
        }
    }

    fn take_secrets(&mut self) -> DeploymentSecrets {
        let users = self
            .users
            .iter_mut()
            .map(|user| UserSecret::new(user.username.clone(), std::mem::take(&mut user.password)))
            .collect();
        DeploymentSecrets::new(std::mem::take(&mut self.root_password), users)
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
        let mode = self.network_mode();
        NetworkConfig {
            port_forwards: mode
                .as_ref()
                .filter(|mode| mode.supports_port_forwarding())
                .map(|_| self.port_list.clone())
                .unwrap_or_default(),
            mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnclassifiedFile {
    pub host_path: String,
    pub default_container_path: String,
    pub assigned_category: Option<crate::domain::nvidia::NvidiaFileCategory>,
    pub custom_destination: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassthroughState {
    pub privileged: bool,
    pub private_users: Option<PrivateUsersMode>,
    pub graphics_acceleration: bool,
    pub gpu_passthrough_all: bool,
    pub discovered_gpus: Vec<HostGpuDevice>,
    pub nvidia_gpu: bool,
    pub nvidia_toolkit_installed: bool,
    pub selected_gpu_nodes: Vec<String>,
    pub wayland_sockets: Vec<HostWaylandSocket>,
    pub bind_mounts: Vec<BindMount>,

    // Advanced NVIDIA passthrough
    pub nvidia_passthrough_mode: crate::domain::nvidia::NvidiaPassthroughMode,
    pub nvidia_gpu_device: String,
    pub nvidia_category_destinations:
        std::collections::BTreeMap<crate::domain::nvidia::NvidiaFileCategory, String>,
    pub nvidia_inject_env: bool,
    pub nvidia_available_devices: Vec<String>,
    pub active_nvidia_categories: Vec<crate::domain::nvidia::NvidiaFileCategory>,
    pub unclassified_files: Vec<UnclassifiedFile>,
    pub hardware_scanning: bool,
}

impl PassthroughState {
    pub fn extract_config(&self) -> PassthroughConfig {
        PassthroughConfig {
            bind_mounts: self.bind_mounts.clone(),
            device_binds: self.selected_gpu_nodes.clone(),
            privileged: self.privileged,
            private_users: self.private_users,
            graphics_acceleration: self.graphics_acceleration,
            gpu_passthrough_all: self.gpu_passthrough_all,
            nvidia_gpu: self.nvidia_gpu && self.nvidia_toolkit_installed,
            nvidia_profile: if self.nvidia_gpu {
                let manual_classifications: Vec<crate::domain::nvidia::ManualClassification> = self
                    .unclassified_files
                    .iter()
                    .filter(|f| f.assigned_category.is_some())
                    .map(|f| crate::domain::nvidia::ManualClassification {
                        host_path: f.host_path.clone(),
                        category: f.assigned_category.clone().unwrap(),
                        destination: if f.custom_destination.is_empty() {
                            f.default_container_path.clone()
                        } else {
                            f.custom_destination.clone()
                        },
                        readonly: f.readonly,
                    })
                    .collect();

                Some(crate::domain::nvidia::NvidiaPassthroughProfile {
                    gpu_device: self.nvidia_gpu_device.clone(),
                    mode: self.nvidia_passthrough_mode.clone(),
                    category_destinations: self.nvidia_category_destinations.clone(),
                    inject_env: self.nvidia_inject_env,
                    manual_classifications,
                })
            } else {
                None
            },
        }
    }
}

/// Holds shared data for the multi-step container creation wizard.
pub struct WizardDraft {
    pub source: SourceState,
    pub basic: BasicState,
    pub storage: StorageState,
    pub user: UserState,
    pub network: NetworkState,
    pub passthrough: PassthroughState,
    pub entries: Vec<MachineEntry>,
    pub images: Vec<ImageEntry>,
    pub host: ProvisioningHostSnapshot,
}

impl WizardDraft {
    pub fn new(
        entries: Vec<MachineEntry>,
        images: Vec<ImageEntry>,
        config: Arc<crate::config::AppConfig>,
        host: ProvisioningHostSnapshot,
    ) -> Self {
        // NVIDIA and GPU discovery is now offloaded to a background task
        let discovered_gpus = vec![];
        let nvidia_available_devices = vec!["all".to_string()];
        let active_nvidia_categories = vec![];
        let (profiles, default_profiles, default_method, default_profile) =
            Self::configured_profiles(&config);
        let default_kind =
            configured_default_source_kind(&profiles, default_method, default_profile);
        let deboot_prefill = method_default(&default_profiles, BootstrapMethod::Debootstrap)
            .and_then(|source| match source {
                RootfsSourceSpec::Debootstrap(spec) => Some(spec.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let pacstrap_prefill = method_default(&default_profiles, BootstrapMethod::Pacstrap)
            .and_then(|source| match source {
                RootfsSourceSpec::Pacstrap(spec) => Some(spec.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let dnf_prefill = method_default(&default_profiles, BootstrapMethod::Dnf5)
            .and_then(|source| match source {
                RootfsSourceSpec::Dnf5(spec) => Some(spec.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let artifact_prefill = method_default(&default_profiles, BootstrapMethod::Artifact)
            .and_then(|source| match source {
                RootfsSourceSpec::Artifact(spec) => Some(spec.expanded_path()),
                _ => None,
            })
            .unwrap_or_default();
        Self {
            source: SourceState {
                kind: default_kind,
                oci_url: "".to_string(),
                oci_read_only: false,
                oci_network: OciNetworkMode::Host,
                deboot_mirror: deboot_prefill.mirror.unwrap_or_default(),
                deboot_suite: deboot_prefill.suite,
                deboot_pkgs: deboot_prefill.packages.join(" "),
                deboot_inherit_default_packages: deboot_prefill.inherit_default_packages,
                pacstrap_pkgs: pacstrap_prefill.packages.join(" "),
                pacstrap_inherit_default_packages: pacstrap_prefill.inherit_default_packages,
                dnf_releasever: dnf_prefill.releasever,
                dnf_pkgs: dnf_prefill.packages.join(" "),
                dnf_inherit_default_packages: dnf_prefill.inherit_default_packages,
                local_path: artifact_prefill,
                clone_source: images
                    .first()
                    .map(|image| image.name.clone())
                    .unwrap_or_default(),
                pull_url: "".to_string(),
                is_pull_raw: false,
                unsafe_tar_accepted_for: None,
                copy_idx: 0,
                profiles,
                default_profiles,
            },
            basic: BasicState {
                name: "".to_string(),
                guest_hostname: "".to_string(),
            },
            storage: StorageState {
                type_idx: 0,
                info: StorageInfo {
                    types: host.storage_backends.clone(),
                    filesystems: host.filesystems.clone(),
                },
                creation_method_idx: 0,
                disk_size: "2G".to_string(),
                disk_fs: DiskImageFilesystem::Ext4,
                disk_partition: true,
                import_path: "".to_string(),
                disk_root_partition: None,
            },
            user: UserState {
                root_password: "".to_string(),
                users: vec![],
            },
            network: {
                let bridges = host.bridges.clone();
                let default_bridge = bridges
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "br0".to_string());

                let physical_interfaces = host.physical_interfaces.clone();
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
                private_users: None,
                graphics_acceleration: false,
                gpu_passthrough_all: false,
                discovered_gpus,
                nvidia_gpu: false,
                nvidia_toolkit_installed: host.nvidia_toolkit_installed,
                selected_gpu_nodes: vec![],
                wayland_sockets: host.wayland_sockets.clone(),
                bind_mounts: vec![],
                nvidia_passthrough_mode: crate::domain::nvidia::NvidiaPassthroughMode::Mirror,
                nvidia_gpu_device: "all".to_string(),
                nvidia_category_destinations: std::collections::BTreeMap::new(),
                nvidia_inject_env: false,
                nvidia_available_devices,
                active_nvidia_categories,
                unclassified_files: vec![],
                hardware_scanning: true,
            },
            entries,
            images,
            host,
        }
    }

    pub fn build_deployment_request(&self) -> DeploymentRequest {
        let source = match self.source.extract_config() {
            SourceConfig::Copy { source_name } => DeploymentSource::Copy { source_name },
            SourceConfig::Oci {
                reference,
                read_only,
                network,
            } => DeploymentSource::Oci {
                reference,
                read_only,
                network,
            },
            SourceConfig::Bootstrap(spec) => DeploymentSource::Bootstrap(spec),
            SourceConfig::Pull { url, is_raw } => DeploymentSource::Pull { url, is_raw },
            SourceConfig::Artifact(spec) => DeploymentSource::Artifact(spec),
        };
        let basic = self.basic.extract_config();
        if !source.supports_rootfs_configuration() {
            let config = match &source {
                DeploymentSource::Copy { .. } => {
                    crate::application::provisioning::MachineProvisioningConfig {
                        name: basic.name,
                        ..Default::default()
                    }
                }
                DeploymentSource::Oci { network, .. } => {
                    crate::application::provisioning::MachineProvisioningConfig {
                        name: basic.name,
                        network: Some(network.into_network_mode()),
                        private_users: Some(PrivateUsersMode::No),
                        boot: false,
                        ..Default::default()
                    }
                }
                DeploymentSource::Bootstrap(_)
                | DeploymentSource::Pull { .. }
                | DeploymentSource::Artifact(_) => unreachable!(
                    "sources with rootfs configuration must use the full request projection"
                ),
            };
            return DeploymentRequest {
                config,
                source,
                storage: DeploymentStorage::Directory,
                nvidia_profile: None,
                wayland: Vec::new(),
                allow_unsafe_remote_tar: false,
            };
        }

        let storage_config = self.storage.extract_config();
        let storage = match storage_config {
            StorageConfig {
                storage_type: StorageBackendKind::Directory,
                ..
            } => DeploymentStorage::Directory,
            StorageConfig {
                storage_type: StorageBackendKind::Subvolume,
                ..
            } => DeploymentStorage::Subvolume,
            StorageConfig {
                storage_type: StorageBackendKind::DiskImage,
                disk_config,
            } => DeploymentStorage::DiskImage(disk_config.unwrap_or_default()),
        };
        let network = self.network.extract_config();
        let users = self.user.extract_config();
        let passthrough = self.passthrough.extract_config();
        let wayland = self
            .user
            .users
            .iter()
            .filter_map(|user| {
                user.wayland
                    .as_ref()
                    .map(|access| access.intent_for(&user.username))
            })
            .collect();
        let config = crate::application::provisioning::MachineProvisioningConfig {
            name: basic.name,
            guest_hostname: basic.guest_hostname,
            network: network.mode,
            port_forwards: network.port_forwards,
            bind_mounts: passthrough.bind_mounts,
            device_binds: passthrough.device_binds,
            readonly_binds: Vec::new(),
            privileged: passthrough.privileged,
            private_users: passthrough.private_users,
            graphics_acceleration: passthrough.graphics_acceleration,
            gpu_passthrough_all: passthrough.gpu_passthrough_all,
            users: users.users,
            nvidia_gpu: passthrough.nvidia_gpu,
            disk_config: match &storage {
                DeploymentStorage::DiskImage(config) => Some(config.clone()),
                DeploymentStorage::Directory | DeploymentStorage::Subvolume => None,
            },
            boot: true,
        };

        DeploymentRequest {
            config,
            source,
            storage,
            nvidia_profile: passthrough.nvidia_profile,
            wayland,
            allow_unsafe_remote_tar: self.source.unsafe_remote_tar_accepted(),
        }
    }

    pub fn take_submission(&mut self, request: DeploymentRequest) -> DeploymentSubmission {
        let secrets = self.user.take_secrets();
        let secrets = if request.source.supports_rootfs_configuration() {
            secrets
        } else {
            drop(secrets);
            DeploymentSecrets::new(String::new(), Vec::new())
        };
        DeploymentSubmission::new(request, secrets)
    }

    fn configured_profiles(
        config: &crate::config::AppConfig,
    ) -> (
        Vec<ConfiguredSourceProfile>,
        Vec<ConfiguredSourceProfile>,
        Option<BootstrapMethod>,
        Option<String>,
    ) {
        let resolved = config.bootstrap.resolve();
        let mut profiles = Vec::new();
        let mut default_profiles = Vec::new();
        for profile in resolved.profiles {
            let crate::config::ResolvedBootstrapProfile {
                method,
                name,
                source,
            } = profile;
            if name.trim().is_empty() || name.chars().any(char::is_control) {
                log::warn!("Ignoring bootstrap profile with invalid name");
                continue;
            }
            let is_default = name == DEFAULT_BOOTSTRAP_PROFILE;
            let validation = if is_default {
                source.validate_default_preset()
            } else {
                source.validate()
            };
            if let Err(error) = validation {
                log::warn!("Ignoring invalid bootstrap profile '{}': {}", name, error);
                continue;
            }
            let target = ConfiguredSourceProfile {
                method,
                name,
                source,
            };
            if is_default {
                default_profiles.push(target);
            } else {
                profiles.push(target);
            }
        }
        (
            profiles,
            default_profiles,
            resolved.default_method,
            resolved.default_profile,
        )
    }

    pub fn update_hardware_data(&mut self, snapshot: HostHardwareSnapshot) {
        self.passthrough.hardware_scanning = false;
        self.passthrough.discovered_gpus = snapshot.gpus;
        self.passthrough.nvidia_available_devices = snapshot.nvidia_devices;
        self.passthrough.active_nvidia_categories = snapshot.active_nvidia_categories;
        // Merge fresh unclassified CDI files with existing user reclassifications
        let fresh_unclassified: Vec<UnclassifiedFile> = snapshot
            .unclassified_nvidia_files
            .into_iter()
            .map(|file| UnclassifiedFile {
                host_path: file.host_path,
                default_container_path: file.default_container_path,
                assigned_category: None,
                custom_destination: String::new(),
                readonly: file.readonly,
            })
            .collect();

        let old_files = std::mem::take(&mut self.passthrough.unclassified_files);
        self.passthrough.unclassified_files = fresh_unclassified
            .into_iter()
            .map(|fresh| {
                old_files
                    .iter()
                    .find(|o| o.host_path == fresh.host_path)
                    .cloned()
                    .unwrap_or(fresh)
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bootstrap::DebootstrapReleaseSignaturePolicy;

    fn test_wayland_access() -> WaylandAccessDraft {
        let source = HostWaylandSocket::from_verified_parts(
            crate::domain::wayland::WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1001".into(),
            "/run/user/1001/wayland-0".into(),
            1001,
            1001,
            1001,
            0o755,
            crate::domain::wayland::SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        WaylandAccessDraft::new(vec![source]).unwrap()
    }

    fn test_source_state() -> SourceState {
        SourceState {
            kind: SourceKind::Copy,
            oci_url: "".into(),
            oci_read_only: false,
            oci_network: OciNetworkMode::Host,
            deboot_mirror: "".into(),
            deboot_suite: "".into(),
            deboot_pkgs: "".into(),
            deboot_inherit_default_packages: true,
            pacstrap_pkgs: "".into(),
            pacstrap_inherit_default_packages: true,
            dnf_releasever: "".into(),
            dnf_pkgs: "".into(),
            dnf_inherit_default_packages: true,
            local_path: "".into(),
            clone_source: "".into(),
            pull_url: "".into(),
            is_pull_raw: false,
            unsafe_tar_accepted_for: None,
            copy_idx: 0,
            profiles: vec![],
            default_profiles: vec![],
        }
    }

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
        assert_eq!(
            state.network_mode(),
            Some(NetworkMode::Bridge("br0".into()))
        );
    }

    #[test]
    fn network_state_drops_port_forwards_for_unsupported_modes() {
        let mut state = NetworkState {
            mode: 0,
            bridge_name: "br0".into(),
            bridge_list: vec![],
            interface_name: "eth0".into(),
            physical_interfaces: vec![],
            port_list: vec![PortForward {
                host: 8080,
                container: 80,
                proto: "tcp".into(),
            }],
        };

        for mode in [0, 1, 4, 5, 6] {
            state.mode = mode;
            assert!(state.extract_config().port_forwards.is_empty());
        }

        state.mode = 2;
        assert_eq!(state.extract_config().port_forwards.len(), 1);
        state.mode = 3;
        assert_eq!(state.extract_config().port_forwards.len(), 1);
    }

    #[test]
    fn test_source_state_externally_managed() {
        let mut state = test_source_state();
        state.kind = SourceKind::Oci;
        assert!(state.is_storage_managed_externally());

        state.kind = SourceKind::Pull;
        state.is_pull_raw = true;
        assert!(state.is_storage_managed_externally());

        state.kind = SourceKind::LocalFile;
        state.local_path = "test.raw".into();
        assert!(state.is_storage_managed_externally());

        state.local_path = "test.tar.gz".into();
        assert!(!state.is_storage_managed_externally());
    }

    #[test]
    fn unsafe_tar_acceptance_is_bound_to_the_current_remote_url() {
        let mut state = test_source_state();
        state.kind = SourceKind::Pull;
        state.pull_url = " https://example.test/rootfs.tar ".into();

        assert!(!state.unsafe_remote_tar_accepted());
        assert!(state.accept_unsafe_remote_tar());
        assert!(state.unsafe_remote_tar_accepted());

        state.pull_url = "https://example.test/other.tar".into();
        assert!(!state.unsafe_remote_tar_accepted());

        state.pull_url = "https://example.test/rootfs.tar".into();
        state.is_pull_raw = true;
        assert!(!state.unsafe_remote_tar_accepted());
    }

    #[test]
    fn oci_source_preserves_systemd_storage_mode() {
        let mut state = test_source_state();
        state.kind = SourceKind::Oci;
        state.oci_url = " docker.io/library/nginx:latest ".into();
        state.oci_read_only = true;

        assert_eq!(
            state.extract_config(),
            SourceConfig::Oci {
                reference: "docker.io/library/nginx:latest".into(),
                read_only: true,
                network: OciNetworkMode::Host,
            }
        );
    }

    #[test]
    fn bootstrap_default_methods_select_builtin_wizard_sources() {
        assert_eq!(
            source_kind_for_method(BootstrapMethod::Debootstrap),
            SourceKind::Debootstrap
        );
        assert_eq!(
            source_kind_for_method(BootstrapMethod::Pacstrap),
            SourceKind::Pacstrap
        );
        assert_eq!(
            source_kind_for_method(BootstrapMethod::Dnf5),
            SourceKind::Dnf5
        );
        assert_eq!(
            source_kind_for_method(BootstrapMethod::Artifact),
            SourceKind::LocalFile
        );
    }

    #[test]
    fn implicit_default_profile_selects_builtin_source() {
        assert_eq!(
            configured_default_source_kind(
                &[],
                Some(BootstrapMethod::Debootstrap),
                Some(DEFAULT_BOOTSTRAP_PROFILE.into()),
            ),
            SourceKind::Debootstrap
        );
    }

    #[test]
    fn policy_only_default_profile_is_applied_to_edited_builtin_source() {
        let config: crate::config::AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"

                [bootstrap.methods.debootstrap]
                default-profile = "default"

                [bootstrap.methods.debootstrap.profiles.default.policy]
                release_signatures = "disabled"
            "#,
        )
        .unwrap();
        let (profiles, defaults, default_method, default_profile) =
            WizardDraft::configured_profiles(&config);

        assert!(profiles.is_empty());
        assert_eq!(defaults.len(), 1);
        assert_eq!(
            configured_default_source_kind(&profiles, default_method, default_profile,),
            SourceKind::Debootstrap
        );

        let mut state = test_source_state();
        state.kind = SourceKind::Debootstrap;
        state.deboot_suite = "noble".into();
        state.deboot_pkgs = "sudo zsh".into();
        state.deboot_inherit_default_packages = false;
        state.default_profiles = defaults;
        let SourceConfig::Bootstrap(BootstrapSpec::Debootstrap(spec)) = state.extract_config()
        else {
            panic!("expected debootstrap source");
        };
        assert_eq!(spec.suite, "noble");
        assert_eq!(spec.packages, ["sudo", "zsh"]);
        assert!(!spec.inherit_default_packages);
        assert_eq!(
            spec.policy.release_signatures,
            DebootstrapReleaseSignaturePolicy::Disabled
        );
    }

    #[test]
    fn debootstrap_builtin_source_has_no_implicit_suite() {
        let mut state = test_source_state();
        state.kind = SourceKind::Debootstrap;

        let SourceConfig::Bootstrap(BootstrapSpec::Debootstrap(spec)) = state.extract_config()
        else {
            panic!("expected debootstrap source");
        };
        assert!(spec.suite.is_empty());
        assert!(spec.validate().is_err());
    }

    #[test]
    fn dnf5_builtin_source_uses_host_repository_without_wizard_state() {
        let mut state = test_source_state();
        state.kind = SourceKind::Dnf5;
        state.dnf_releasever = "43".into();
        state.dnf_pkgs = "systemd".into();

        let SourceConfig::Bootstrap(BootstrapSpec::Dnf5(spec)) = state.extract_config() else {
            panic!("expected dnf5 source");
        };
        assert_eq!(spec.repository, Dnf5RepositorySource::Host);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn incomplete_named_profile_is_not_exposed_as_a_source() {
        let config: crate::config::AppConfig = toml::from_str(
            r#"
                [bootstrap.methods.dnf5.profiles.incomplete]
                releasever = "43"
            "#,
        )
        .unwrap();

        let (profiles, defaults, _, _) = WizardDraft::configured_profiles(&config);
        assert!(profiles.is_empty());
        assert!(defaults.is_empty());
    }

    #[test]
    fn storage_state_applies_manual_root_only_to_imports() {
        let mut state = StorageState {
            type_idx: 0,
            info: StorageInfo {
                types: vec![(StorageBackendKind::DiskImage, true)],
                filesystems: vec![(DiskImageFilesystem::Ext4, true)],
            },
            creation_method_idx: 1,
            disk_size: "2G".into(),
            disk_fs: DiskImageFilesystem::Ext4,
            disk_partition: true,
            import_path: "/tmp/test.raw".into(),
            disk_root_partition: Some(DiskImagePartition::new(2).unwrap()),
        };

        let imported = state.extract_config().disk_config.unwrap();
        assert_eq!(imported.root_partition.unwrap().number(), 2);

        state.creation_method_idx = 0;
        let created = state.extract_config().disk_config.unwrap();
        assert_eq!(created.root_partition, None);
    }

    #[test]
    fn taking_user_secrets_redacts_debug_and_clears_the_draft() {
        let mut state = UserState {
            root_password: "root-sentinel".into(),
            users: vec![UserDraft {
                username: "alice".into(),
                password: "user-sentinel".into(),
                sudoer: false,
                shell: "/bin/bash".into(),
                wayland: None,
            }],
        };

        let state_debug = format!("{state:?}");
        assert!(!state_debug.contains("root-sentinel"));
        assert!(!state_debug.contains("user-sentinel"));

        let secrets = state.take_secrets();
        assert!(state.root_password.is_empty());
        assert!(state.users[0].password.is_empty());
        let secret_debug = format!("{secrets:?}");
        assert!(!secret_debug.contains("root-sentinel"));
        assert!(!secret_debug.contains("user-sentinel"));
    }

    #[test]
    fn test_passthrough_config_logic() {
        let state = PassthroughState {
            privileged: true,
            private_users: None,
            graphics_acceleration: true,
            gpu_passthrough_all: true,
            discovered_gpus: vec![],
            nvidia_gpu: true,
            nvidia_toolkit_installed: true,
            selected_gpu_nodes: vec![],
            wayland_sockets: vec![],
            bind_mounts: vec![],
            nvidia_passthrough_mode: crate::domain::nvidia::NvidiaPassthroughMode::Mirror,
            nvidia_gpu_device: "all".to_string(),
            nvidia_category_destinations: std::collections::BTreeMap::new(),
            nvidia_inject_env: false,
            nvidia_available_devices: vec!["all".to_string()],
            active_nvidia_categories: vec![],
            unclassified_files: vec![],
            hardware_scanning: false,
        };

        let cfg = state.extract_config();
        assert!(cfg.gpu_passthrough_all);

        // Nvidia GPU only if toolkit installed
        let mut state_no_toolkit = state.clone();
        state_no_toolkit.nvidia_toolkit_installed = false;
        let cfg = state_no_toolkit.extract_config();
        assert!(!cfg.nvidia_gpu);
    }

    #[test]
    fn deployment_request_drops_wayland_grant_for_sources_without_user_setup() {
        let mut draft = WizardDraft::new(
            vec![],
            vec![],
            Arc::new(crate::config::AppConfig::default()),
            ProvisioningHostSnapshot::default(),
        );
        draft.user.users.push(UserDraft {
            username: "lumie".into(),
            password: String::new(),
            sudoer: false,
            shell: "/bin/bash".into(),
            wayland: Some(test_wayland_access()),
        });

        draft.source.kind = SourceKind::Pacstrap;
        let request = draft.build_deployment_request();
        assert_eq!(request.wayland.len(), 1);
        assert_eq!(request.config.users[0].uid, Some(1001));

        draft.source.kind = SourceKind::Copy;
        let request = draft.build_deployment_request();
        assert!(request.wayland.is_empty());
        assert!(request.config.users.is_empty());
        assert!(request.config.bind_mounts.is_empty());
        assert_eq!(request.storage, DeploymentStorage::Directory);

        draft.source.kind = SourceKind::Oci;
        let request = draft.build_deployment_request();
        assert!(request.wayland.is_empty());
        assert!(request.config.users.is_empty());
        assert!(request.config.bind_mounts.is_empty());
        assert_eq!(request.config.private_users, Some(PrivateUsersMode::No));
        assert!(!request.config.boot);
    }

    #[test]
    fn copy_submission_discards_hidden_account_secrets() {
        let mut draft = WizardDraft::new(
            vec![],
            vec![],
            Arc::new(crate::config::AppConfig::default()),
            ProvisioningHostSnapshot::default(),
        );
        draft.source.kind = SourceKind::Copy;
        draft.user.root_password = "root-sentinel".into();
        draft.user.users.push(UserDraft {
            username: "alice".into(),
            password: "user-sentinel".into(),
            sudoer: false,
            shell: "/bin/bash".into(),
            wayland: None,
        });

        let request = draft.build_deployment_request();
        let submission = draft.take_submission(request);
        assert!(submission.validate_secrets().is_ok());
        let (_, secrets) = submission.into_parts();
        assert!(!secrets.has_account_changes());
        assert!(draft.user.root_password.is_empty());
        assert!(draft.user.users[0].password.is_empty());
    }
}
