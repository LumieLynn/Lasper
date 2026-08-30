use super::{DeploymentError, DeploymentRequest};
use crate::domain::nvidia::NvidiaFileCategory;
use crate::domain::storage::{DiskImageFilesystem, DiskImagePartition};
use crate::domain::wayland::HostWaylandSocket;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StorageBackendKind {
    Directory,
    Subvolume,
    DiskImage,
}

impl StorageBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directory => "Directory",
            Self::Subvolume => "Btrfs Subvolume",
            Self::DiskImage => "Disk Image (Raw/Block)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCapability {
    pub available: bool,
    pub reason: Option<String>,
}

impl HostCapability {
    pub fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

impl Default for HostCapability {
    fn default() -> Self {
        Self::unavailable("capability was not inspected")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProvisioningHostSnapshot {
    pub storage_backends: Vec<(StorageBackendKind, bool)>,
    pub tools: BTreeMap<String, bool>,
    pub filesystems: Vec<(DiskImageFilesystem, bool)>,
    pub oci: HostCapability,
    pub bridges: Vec<String>,
    pub physical_interfaces: Vec<String>,
    pub wayland_sockets: Vec<HostWaylandSocket>,
    pub nvidia_toolkit_installed: bool,
}

impl Default for ProvisioningHostSnapshot {
    fn default() -> Self {
        Self {
            storage_backends: vec![
                (StorageBackendKind::Directory, true),
                (StorageBackendKind::DiskImage, true),
                (StorageBackendKind::Subvolume, false),
            ],
            tools: BTreeMap::new(),
            filesystems: DiskImageFilesystem::ALL
                .iter()
                .copied()
                .map(|filesystem| (filesystem, false))
                .collect(),
            oci: HostCapability::default(),
            bridges: Vec::new(),
            physical_interfaces: Vec::new(),
            wayland_sockets: Vec::new(),
            nvidia_toolkit_installed: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGpuDevice {
    pub display_name: String,
    pub driver_type: String,
    pub nodes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnclassifiedNvidiaFile {
    pub host_path: String,
    pub default_container_path: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostHardwareSnapshot {
    pub nvidia_devices: Vec<String>,
    pub gpus: Vec<HostGpuDevice>,
    pub active_nvidia_categories: Vec<NvidiaFileCategory>,
    pub unclassified_nvidia_files: Vec<UnclassifiedNvidiaFile>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterfaceValidation {
    Valid,
    Warning(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePartitionInfo {
    pub number: DiskImagePartition,
    pub type_label: String,
    pub current_architecture_root: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePartitionProbe {
    pub label: String,
    pub partitions: Vec<ImagePartitionInfo>,
}

#[async_trait]
pub trait ProvisioningPreparationPort: Send + Sync {
    async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError>;

    async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError>;

    async fn validate_interface(
        &self,
        name: &str,
        bridge_mode: bool,
    ) -> Result<InterfaceValidation, DeploymentError>;

    fn probe_image(&self, path: &Path) -> Result<Option<ImagePartitionProbe>, DeploymentError>;

    fn preview(&self, request: &DeploymentRequest) -> String;
}

pub struct ProvisioningPreparationService {
    port: Arc<dyn ProvisioningPreparationPort>,
}

impl ProvisioningPreparationService {
    pub fn new(port: Arc<dyn ProvisioningPreparationPort>) -> Self {
        Self { port }
    }

    pub async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
        self.port.inspect_host().await
    }

    pub async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
        self.port.discover_hardware().await
    }

    pub async fn validate_interface(
        &self,
        name: &str,
        bridge_mode: bool,
    ) -> Result<InterfaceValidation, DeploymentError> {
        self.port.validate_interface(name, bridge_mode).await
    }

    pub fn probe_image(&self, path: &Path) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
        self.port.probe_image(path)
    }

    pub fn preview(&self, request: &DeploymentRequest) -> String {
        self.port.preview(request)
    }
}
