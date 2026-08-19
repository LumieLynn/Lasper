use crate::nspawn::errors::Result;
use crate::nspawn::models::{
    AllowedSignal, ContainerEntry, ImageEntry, MachineProperties, RuntimeSnapshot, StatusUpdate,
};

/// Read-only runtime discovery, inspection, and observation.
///
/// Application services depend on this port without gaining mutation methods.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait RuntimeSource: Send + Sync + 'static {
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>>;
    async fn list_images(&self) -> Result<Vec<ImageEntry>>;
    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        let (machines, images) = tokio::try_join!(self.list_machines(), self.list_images())?;
        Ok(RuntimeSnapshot::new(machines, images))
    }
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()>;
}

/// Typed machine and image mutations.
///
/// Keeping this separate from [`RuntimeSource`] prevents read-only consumers
/// from accidentally acquiring control authority through a broad backend.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineControl: Send + Sync + 'static {
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()>;
    async fn reload_daemon(&self) -> Result<()>;
}

/// Unified backend for communicating with systemd-machined.
///
/// DBus observers publish dirty hints from signals, while CLI observers
/// publish complete snapshots from polling. Consumers do not need to repeat
/// discovery when a backend already produced a snapshot.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ContainerBackend: Send + Sync + 'static {
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>>;
    /// Return persistent machine images independently from running machines.
    async fn list_images(&self) -> Result<Vec<ImageEntry>>;
    /// Return one normalized machine/image view from this backend.
    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        let (machines, images) = tokio::try_join!(self.list_machines(), self.list_images())?;
        Ok(RuntimeSnapshot::new(machines, images))
    }
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<()>;
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
    async fn reload_daemon(&self) -> Result<()>;
    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()>;
}

/// Compatibility adapter while concrete communication backends are migrated
/// away from the former combined [`ContainerBackend`] surface.
#[derive(Clone)]
pub struct RuntimeAdapter<T> {
    backend: T,
}

impl<T> RuntimeAdapter<T> {
    pub fn new(backend: T) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl<T> RuntimeSource for RuntimeAdapter<T>
where
    T: ContainerBackend + Send + Sync + 'static,
{
    async fn is_available(&self) -> bool {
        self.backend.is_available().await
    }

    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        self.backend.list_machines().await
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        self.backend.list_images().await
    }

    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        self.backend.snapshot().await
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        self.backend.get_properties(name).await
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        self.backend.watch_events(tx).await
    }
}

#[derive(Clone)]
pub struct ControlAdapter<T> {
    backend: T,
}

impl<T> ControlAdapter<T> {
    pub fn new(backend: T) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl<T> MachineControl for ControlAdapter<T>
where
    T: ContainerBackend + Send + Sync + 'static,
{
    async fn start(&self, name: &str) -> Result<()> {
        self.backend.start(name).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.backend.terminate(name).await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.backend.poweroff(name).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.backend.reboot(name).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.backend.enable(name).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.backend.disable(name).await
    }

    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        self.backend.kill(name, signal).await
    }

    async fn reload_daemon(&self) -> Result<()> {
        self.backend.reload_daemon().await
    }
}
