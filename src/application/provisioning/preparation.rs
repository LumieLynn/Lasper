use super::{DeploymentError, DeploymentRequest};
use crate::domain::nvidia::NvidiaFileCategory;
use crate::domain::storage::{DiskImageFilesystem, DiskImagePartition};
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};
use crate::domain::x11::HostX11Socket;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const HOST_INSPECTION_TIMEOUT: Duration = Duration::from_secs(30);
const HARDWARE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const INTERFACE_VALIDATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Read-only preparation may be cancelled without abandoning a host mutation.
/// Adapters remain responsible for bounding any external command they start.
pub(super) async fn read_query<T>(
    label: &str,
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T, DeploymentError>>,
) -> Result<T, DeploymentError> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        DeploymentError::failed(format!("{label} timed out after {}s", timeout.as_secs()))
    })?
}

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
    pub preferred_wayland_display: Option<WaylandDisplay>,
    pub x11_sockets: Vec<HostX11Socket>,
    pub preferred_x11_display: Option<u16>,
    /// The standard pathname namespace passed the same ownership/mode checks
    /// used by host X11 discovery; it may still contain no live socket.
    pub x11_socket_directory_available: bool,
    pub nvidia_cdi_available: bool,
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
            preferred_wayland_display: None,
            x11_sockets: Vec::new(),
            preferred_x11_display: None,
            x11_socket_directory_available: false,
            nvidia_cdi_available: false,
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
        read_query(
            "Host inspection",
            HOST_INSPECTION_TIMEOUT,
            self.port.inspect_host(),
        )
        .await
    }

    pub async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
        read_query(
            "Hardware discovery",
            HARDWARE_DISCOVERY_TIMEOUT,
            self.port.discover_hardware(),
        )
        .await
    }

    pub async fn validate_interface(
        &self,
        name: &str,
        bridge_mode: bool,
    ) -> Result<InterfaceValidation, DeploymentError> {
        read_query(
            "Interface validation",
            INTERFACE_VALIDATION_TIMEOUT,
            self.port.validate_interface(name, bridge_mode),
        )
        .await
    }

    pub fn probe_image(&self, path: &Path) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
        self.port.probe_image(path)
    }

    pub fn preview(&self, request: &DeploymentRequest) -> String {
        self.port.preview(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PendingPreparation(Arc<AtomicUsize>);

    struct QueryGuard(Arc<AtomicUsize>);
    impl Drop for QueryGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl PendingPreparation {
        async fn pending<T>(&self) -> T {
            let _guard = QueryGuard(self.0.clone());
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ProvisioningPreparationPort for PendingPreparation {
        async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
            self.pending().await
        }

        async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
            self.pending().await
        }

        async fn validate_interface(
            &self,
            _: &str,
            _: bool,
        ) -> Result<InterfaceValidation, DeploymentError> {
            self.pending().await
        }

        fn probe_image(&self, _: &Path) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
            unreachable!()
        }

        fn preview(&self, _: &DeploymentRequest) -> String {
            unreachable!()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_preparation_query_releases_a_stalled_read_on_deadline() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let service =
            ProvisioningPreparationService::new(Arc::new(PendingPreparation(cancelled.clone())));
        let started = tokio::time::Instant::now();
        let error = service.validate_interface("br0", true).await.unwrap_err();
        assert!(error.to_string().contains("Interface validation timed out"));
        assert_eq!(started.elapsed(), INTERFACE_VALIDATION_TIMEOUT);
        let started = tokio::time::Instant::now();
        assert!(service
            .inspect_host()
            .await
            .unwrap_err()
            .to_string()
            .contains("Host inspection timed out"));
        assert_eq!(started.elapsed(), HOST_INSPECTION_TIMEOUT);
        let started = tokio::time::Instant::now();
        assert!(service
            .discover_hardware()
            .await
            .unwrap_err()
            .to_string()
            .contains("Hardware discovery timed out"));
        assert_eq!(started.elapsed(), HARDWARE_DISCOVERY_TIMEOUT);
        assert_eq!(cancelled.load(Ordering::Relaxed), 3);
    }
}
