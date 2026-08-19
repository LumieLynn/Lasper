//! [`DaemonBackend`] — a [`ContainerBackend`] that proxies every call
//! through the elevated daemon's root DBus connection.
//!
//! In Elevated mode the daemon runs as root, so its zbus `Connection::system()`
//! has full privileges and polkit passes automatically. This backend sends
//! high-level `ContainerBackend` operations over the daemon's JSON-RPC link
//! instead of connecting to DBus directly.

use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    AllowedSignal, ContainerEntry, ImageEntry, ImageName, MachineName, MachineProperties,
    StatusUpdate,
};
use crate::nspawn::ops::image_lifecycle::{
    ImageControlOutcome, ImageRemoveRequest, ImageRemoveTransport,
};
use crate::nspawn::ops::system_operation::SystemOperation;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use std::sync::Arc;

#[derive(Clone)]
pub struct DaemonBackend {
    daemon: Arc<ElevatedDaemon>,
}

impl DaemonBackend {
    pub fn new(daemon: Arc<ElevatedDaemon>) -> Self {
        Self { daemon }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.daemon
            .rpc_call(method, params)
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("daemon"), e))
    }

    async fn call_system_operation(&self, operation: SystemOperation) -> Result<()> {
        let params = serde_json::to_value(operation)
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
        self.call("dbus_system_operation", params).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ContainerBackend for DaemonBackend {
    async fn is_available(&self) -> bool {
        self.call("dbus_is_available", serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        let json = self
            .call("dbus_list_machines", serde_json::json!({}))
            .await?;
        serde_json::from_value(json).map_err(|e| {
            NspawnError::Dbus(zbus::Error::Failure(format!(
                "failed to deserialize list_machines response: {}",
                e
            )))
        })
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let json = self.call("dbus_list_images", serde_json::json!({})).await?;
        serde_json::from_value(json).map_err(|e| {
            NspawnError::Dbus(zbus::Error::Failure(format!(
                "failed to deserialize list_images response: {}",
                e
            )))
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Start {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Terminate {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Poweroff {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Reboot {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Enable {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.call_system_operation(SystemOperation::Disable {
            machine: machine_name(name)?,
        })
        .await
    }

    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        self.call_system_operation(SystemOperation::Kill {
            machine: machine_name(name)?,
            signal,
        })
        .await
    }

    async fn remove(&self, name: &str) -> Result<()> {
        let image =
            ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))?;
        let value = self
            .call(
                "image_remove",
                serde_json::to_value(ImageRemoveRequest {
                    image,
                    transport: ImageRemoveTransport::Dbus,
                })
                .map_err(|error| NspawnError::Runtime(error.to_string()))?,
            )
            .await?;
        let outcome: ImageControlOutcome = serde_json::from_value(value)
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
        match outcome {
            ImageControlOutcome::Removed => Ok(()),
            ImageControlOutcome::NotAttempted { reason } => Err(NspawnError::Runtime(format!(
                "image removal not attempted: {reason}"
            ))),
            ImageControlOutcome::Rejected { reason, .. } => Err(NspawnError::Validation(reason)),
            ImageControlOutcome::Failed { reason } => Err(NspawnError::Runtime(reason)),
            ImageControlOutcome::OutcomeUnknown { reason } => Err(NspawnError::Io(
                std::path::PathBuf::from("daemon image removal"),
                std::io::Error::new(std::io::ErrorKind::Interrupted, reason),
            )),
        }
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let json = self
            .call("dbus_get_properties", serde_json::json!({"name": name}))
            .await?;
        serde_json::from_value(json).map_err(|e| {
            NspawnError::Dbus(zbus::Error::Failure(format!(
                "failed to deserialize get_properties response: {}",
                e
            )))
        })
    }

    async fn reload_daemon(&self) -> Result<()> {
        self.call_system_operation(SystemOperation::ReloadDaemon)
            .await
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        let mut event_rx = self.daemon.subscribe_events();
        loop {
            match event_rx.recv().await {
                Ok(()) => {
                    if tx.send(StatusUpdate::Dirty).await.is_err() {
                        break; // receiver dropped
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("DaemonBackend event receiver lagged by {} messages", n);
                    // Still nudge — something changed
                    let _ = tx.send(StatusUpdate::Dirty).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::error!("DaemonBackend event channel closed");
                    return Err(NspawnError::Runtime(
                        "elevated daemon event channel closed".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}
