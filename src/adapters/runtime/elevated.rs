//! [`DaemonBackend`] — a [`RuntimeSource`] that proxies runtime reads
//! through the elevated daemon's root D-Bus connection.
//!
//! In Elevated mode the daemon runs as root, so its zbus `Connection::system()`
//! has full privileges and polkit passes automatically. Runtime queries are
//! read-only here; machine and image mutations use typed lifecycle requests.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::runtime::source::RuntimeSource;
use crate::domain::inspection::MachineProperties;
use crate::domain::runtime::{ImageEntry, MachineEntry, StatusUpdate};
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
