pub use crate::nspawn::adapters::config::builder::{
    BasicConfig, ContainerConfigBuilder, ContainerConfigWithPreview, NetworkConfig,
    PassthroughConfig, SourceConfig, SourceKind, StorageConfig, UserConfig,
};
use crate::nspawn::adapters::storage::{StorageBackend, StorageInfo, StorageType};
use crate::nspawn::models::ContainerEntry;
use crate::nspawn::models::{
    ArtifactSpec, BootstrapMethod, BootstrapSpec, RootfsSourceSpec, DEFAULT_BOOTSTRAP_PROFILE,
};
use crate::nspawn::models::{BindMount, CreateUser, NetworkMode, PortForward};
use crate::nspawn::models::{DiskImageFilesystem, DiskImagePartition};
use crate::nspawn::ops::provision::{DeployLogEvent, Deployer};
use crate::nspawn::ops::PermissionLevel;
use crate::nspawn::sys::ExecutionContext;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::broadcast;

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
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub deboot_pkgs: String,
    pub pacstrap_pkgs: String,
    pub dnf_releasever: String,
    pub dnf_pkgs: String,
    pub local_path: String,
    pub clone_source: String,
    pub pull_url: String,
    pub is_pull_raw: bool,
    pub copy_idx: usize,
    pub profiles: Vec<ConfiguredSourceProfile>,
    pub default_profiles: Vec<ConfiguredSourceProfile>,
}

impl SourceState {
    pub fn extract_config(&self) -> SourceConfig {
        match &self.kind {
            SourceKind::Copy => SourceConfig::Copy {
                source_name: self.clone_source.clone(),
            },
            SourceKind::Oci => SourceConfig::Oci {
                reference: self.oci_url.trim().into(),
                read_only: self.oci_read_only,
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
                if spec.repository == crate::nspawn::models::Dnf5RepositorySource::Unspecified {
                    spec.repository = crate::nspawn::models::Dnf5RepositorySource::Host;
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
            disk_config: if st == StorageType::DiskImage {
                let source = if self.creation_method_idx == 1 {
                    crate::nspawn::models::DiskImageSource::ImportExisting {
                        path: self.import_path.clone(),
                    }
                } else {
                    crate::nspawn::models::DiskImageSource::CreateNew {
                        size: self.disk_size.clone(),
                        fs_type: self.disk_fs,
                    }
                };

                Some(crate::nspawn::models::DiskImageConfig {
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
pub struct UnclassifiedFile {
    pub host_path: String,
    pub default_container_path: String,
    pub assigned_category: Option<crate::nspawn::platform::nvidia::classify::NvidiaFileCategory>,
    pub custom_destination: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PassthroughState {
    pub privileged: bool,
    pub private_users: Option<String>,
    pub graphics_acceleration: bool,
    pub wayland_socket: Option<String>,
    pub discovered_gpus: Vec<crate::nspawn::platform::gpu::GpuDevice>,
    pub nvidia_gpu: bool,
    pub nvidia_toolkit_installed: bool,
    pub selected_gpu_nodes: Vec<String>,
    pub wayland_sockets: Vec<String>,
    pub bind_mounts: Vec<BindMount>,

    // Advanced NVIDIA passthrough
    pub nvidia_passthrough_mode: crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode,
    pub nvidia_gpu_device: String,
    pub nvidia_category_destinations: std::collections::HashMap<
        crate::nspawn::platform::nvidia::classify::NvidiaFileCategory,
        String,
    >,
    pub nvidia_inject_env: bool,
    pub nvidia_available_devices: Vec<String>,
    pub active_nvidia_categories:
        Vec<crate::nspawn::platform::nvidia::classify::NvidiaFileCategory>,
    pub unclassified_files: Vec<UnclassifiedFile>,
    pub hardware_scanning: bool,
}

impl PassthroughState {
    pub fn extract_config(&self, mode: Option<NetworkMode>) -> PassthroughConfig {
        let is_host_nw = matches!(mode, Some(NetworkMode::Host));
        PassthroughConfig {
            bind_mounts: self.bind_mounts.clone(),
            device_binds: self.selected_gpu_nodes.clone(),
            privileged: self.privileged,
            private_users: self.private_users.clone(),
            graphics_acceleration: self.graphics_acceleration,
            wayland_socket: if is_host_nw {
                self.wayland_socket.clone()
            } else {
                None
            },
            nvidia_gpu: self.nvidia_gpu && self.nvidia_toolkit_installed,
            nvidia_profile: if self.nvidia_gpu {
                let manual_classifications: Vec<
                    crate::nspawn::platform::nvidia::profile::ManualClassification,
                > = self
                    .unclassified_files
                    .iter()
                    .filter(|f| f.assigned_category.is_some())
                    .map(
                        |f| crate::nspawn::platform::nvidia::profile::ManualClassification {
                            host_path: f.host_path.clone(),
                            category: f.assigned_category.clone().unwrap(),
                            destination: if f.custom_destination.is_empty() {
                                f.default_container_path.clone()
                            } else {
                                f.custom_destination.clone()
                            },
                            readonly: f.readonly,
                        },
                    )
                    .collect();

                Some(
                    crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile {
                        gpu_device: self.nvidia_gpu_device.clone(),
                        mode: self.nvidia_passthrough_mode.clone(),
                        category_destinations: self.nvidia_category_destinations.clone(),
                        inject_env: self.nvidia_inject_env,
                        manual_classifications,
                    },
                )
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewState {
    pub preview: String,
}

use std::cell::RefCell;

pub struct DeployState {
    pub log_tx: broadcast::Sender<DeployLogEvent>,
    pub log_rx: RefCell<Option<broadcast::Receiver<DeployLogEvent>>>,
    pub done: Arc<AtomicBool>,
    pub success: Arc<AtomicBool>,
}

impl Clone for DeployState {
    fn clone(&self) -> Self {
        Self {
            log_tx: self.log_tx.clone(),
            log_rx: RefCell::new(Some(self.log_tx.subscribe())),
            done: self.done.clone(),
            success: self.success.clone(),
        }
    }
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
    pub image_names: Vec<String>,
    pub xdg_runtime: Option<String>,
    pub permission_level: PermissionLevel,
    pub exec_ctx: Arc<ExecutionContext>,
}

impl WizardContext {
    pub async fn new(
        entries: Vec<ContainerEntry>,
        image_names: Vec<String>,
        permission_level: PermissionLevel,
        exec_ctx: Arc<ExecutionContext>,
        config: Arc<crate::config::AppConfig>,
    ) -> Self {
        let xdg_runtime = crate::nspawn::platform::capabilities::get_xdg_runtime()
            .await
            .ok();
        let nvidia_toolkit_installed = crate::nspawn::platform::nvidia::nvidia_ctk_available();
        let wayland_sockets =
            crate::nspawn::platform::capabilities::scan_available_wayland_sockets().await;

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
                deboot_mirror: deboot_prefill.mirror.unwrap_or_default(),
                deboot_suite: deboot_prefill.suite,
                deboot_pkgs: deboot_prefill.packages.join(" "),
                pacstrap_pkgs: pacstrap_prefill.packages.join(" "),
                dnf_releasever: dnf_prefill.releasever,
                dnf_pkgs: dnf_prefill.packages.join(" "),
                local_path: artifact_prefill,
                clone_source: entries.first().map(|e| e.name.clone()).unwrap_or_default(),
                pull_url: "".to_string(),
                is_pull_raw: false,
                copy_idx: 0,
                profiles,
                default_profiles,
            },
            basic: BasicState {
                name: "".to_string(),
                hostname: "".to_string(),
            },
            storage: StorageState {
                type_idx: 0,
                info: crate::nspawn::adapters::storage::detect::detect_available_storage_types()
                    .await,
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
                let bridges = crate::nspawn::platform::network::detect_bridges().await;
                let default_bridge = bridges
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "br0".to_string());

                let physical_interfaces =
                    crate::nspawn::platform::network::detect_physical_interfaces().await;
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
                wayland_socket: None,
                discovered_gpus,
                nvidia_gpu: false,
                nvidia_toolkit_installed,
                selected_gpu_nodes: vec![],
                wayland_sockets,
                bind_mounts: vec![],
                nvidia_passthrough_mode:
                    crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode::Mirror,
                nvidia_gpu_device: "all".to_string(),
                nvidia_category_destinations: std::collections::HashMap::new(),
                nvidia_inject_env: false,
                nvidia_available_devices,
                active_nvidia_categories,
                unclassified_files: vec![],
                hardware_scanning: true,
            },
            review: ReviewState {
                preview: "".to_string(),
            },
            deploy: {
                let (log_tx, log_rx) = broadcast::channel(1000);
                DeployState {
                    log_tx,
                    log_rx: RefCell::new(Some(log_rx)),
                    done: Arc::new(AtomicBool::new(false)),
                    success: Arc::new(AtomicBool::new(false)),
                }
            },
            entries,
            image_names,
            xdg_runtime,
            permission_level,
            exec_ctx,
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
        self.builder().get_deployer_and_storage(
            system_operations,
            nspawn,
            systemd_unit,
            managed_storage,
            bootstrap,
            image_import,
            oci_pull,
        )
    }

    pub fn update_hardware_data(
        &mut self,
        state: crate::nspawn::platform::nvidia::state::NvidiaState,
        devices: Vec<String>,
        gpus: Vec<crate::nspawn::platform::gpu::GpuDevice>,
    ) {
        self.passthrough.hardware_scanning = false;
        self.passthrough.discovered_gpus = gpus;
        self.passthrough.nvidia_available_devices = devices;
        self.passthrough.active_nvidia_categories =
            crate::nspawn::platform::nvidia::classify::detect_active_categories(
                &state.classified_entries,
            );
        // Merge fresh unclassified CDI files with existing user reclassifications
        let fresh_unclassified: Vec<UnclassifiedFile> = state
            .binds
            .iter()
            .filter(|b| {
                b.readonly
                    && crate::nspawn::platform::nvidia::classify::classify_path(&b.container_path)
                        .is_none()
            })
            .map(|b| UnclassifiedFile {
                host_path: b.host_path.clone(),
                default_container_path: b.container_path.clone(),
                assigned_category: None,
                custom_destination: String::new(),
                readonly: true,
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

    fn test_source_state() -> SourceState {
        SourceState {
            kind: SourceKind::Copy,
            oci_url: "".into(),
            oci_read_only: false,
            deboot_mirror: "".into(),
            deboot_suite: "".into(),
            deboot_pkgs: "".into(),
            pacstrap_pkgs: "".into(),
            dnf_releasever: "".into(),
            dnf_pkgs: "".into(),
            local_path: "".into(),
            clone_source: "".into(),
            pull_url: "".into(),
            is_pull_raw: false,
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
            WizardContext::configured_profiles(&config);

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
        state.default_profiles = defaults;
        let SourceConfig::Bootstrap(BootstrapSpec::Debootstrap(spec)) = state.extract_config()
        else {
            panic!("expected debootstrap source");
        };
        assert_eq!(spec.suite, "noble");
        assert_eq!(spec.packages, ["sudo", "zsh"]);
        assert_eq!(
            spec.policy.release_signatures,
            crate::nspawn::models::DebootstrapReleaseSignaturePolicy::Disabled
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
        assert_eq!(
            spec.repository,
            crate::nspawn::models::Dnf5RepositorySource::Host
        );
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

        let (profiles, defaults, _, _) = WizardContext::configured_profiles(&config);
        assert!(profiles.is_empty());
        assert!(defaults.is_empty());
    }

    #[test]
    fn storage_state_applies_manual_root_only_to_imports() {
        let mut state = StorageState {
            type_idx: 0,
            info: StorageInfo {
                types: vec![(StorageType::DiskImage, true)],
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
    fn test_passthrough_config_logic() {
        let state = PassthroughState {
            privileged: true,
            private_users: None,
            graphics_acceleration: true,
            wayland_socket: Some("wayland-0".into()),
            discovered_gpus: vec![],
            nvidia_gpu: true,
            nvidia_toolkit_installed: true,
            selected_gpu_nodes: vec![],
            wayland_sockets: vec![],
            bind_mounts: vec![],
            nvidia_passthrough_mode:
                crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode::Mirror,
            nvidia_gpu_device: "all".to_string(),
            nvidia_category_destinations: std::collections::HashMap::new(),
            nvidia_inject_env: false,
            nvidia_available_devices: vec!["all".to_string()],
            active_nvidia_categories: vec![],
            unclassified_files: vec![],
            hardware_scanning: false,
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
