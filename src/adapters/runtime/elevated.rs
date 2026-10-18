//! [`DaemonBackend`] — a [`RuntimeSource`] that proxies runtime reads
//! through the elevated daemon's root D-Bus connection.
//!
//! In Elevated mode the daemon runs as root, so its zbus `Connection::system()`
//! has full privileges and polkit passes automatically. Runtime queries are
//! read-only here; machine and image mutations use typed lifecycle requests.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::runtime::source::RuntimeSource;
use crate::application::runtime::{RuntimeChangePort, RuntimeError, RuntimeResult};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry};
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
}

#[async_trait::async_trait]
impl RuntimeSource for DaemonBackend {
    async fn get_unit_properties(&self, machine: &MachineName) -> Result<MachineProperties> {
        let json = self
            .call("dbus_get_unit_properties", serde_json::json!(machine))
            .await?;
        Ok(serde_json::from_value(json)?)
    }

    async fn is_available(&self) -> bool {
        self.call("dbus_is_available", serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    async fn list_machines(&self) -> Result<Vec<MachineEntry>> {
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

    async fn get_properties(
        &self,
        name: &str,
        include_nspawn_unit: bool,
    ) -> Result<MachineProperties> {
        let json = self
            .call(
                "dbus_get_properties",
                serde_json::to_value(crate::ipc::protocol::InspectMachineRequest {
                    machine: crate::domain::machine::MachineName::new(name)
                        .map_err(|error| NspawnError::Validation(error.to_string()))?,
                    include_nspawn_unit,
                })?,
            )
            .await?;
        serde_json::from_value(json).map_err(|e| {
            NspawnError::Dbus(zbus::Error::Failure(format!(
                "failed to deserialize get_properties response: {}",
                e
            )))
        })
    }
}

#[async_trait::async_trait]
impl RuntimeChangePort for DaemonBackend {
    async fn watch_changes(&self, changed: tokio::sync::watch::Sender<()>) -> RuntimeResult<()> {
        let mut event_rx = self.daemon.subscribe_events();
        loop {
            match event_rx.recv().await {
                Ok(()) => {
                    if changed.send(()).is_err() {
                        break; // receiver dropped
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("DaemonBackend event receiver lagged by {} messages", n);
                    if changed.send(()).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::error!("DaemonBackend event channel closed");
                    return Err(RuntimeError::unavailable(
                        "elevated daemon event channel closed",
                    ));
                }
            }
        }
        Ok(())
    }
}
