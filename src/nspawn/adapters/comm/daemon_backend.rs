//! [`DaemonBackend`] — a [`ContainerBackend`] that proxies every call
//! through the elevated daemon's root DBus connection.
//!
//! In Elevated mode the daemon runs as root, so its zbus `Connection::system()`
//! has full privileges and polkit passes automatically. This backend sends
//! high-level `ContainerBackend` operations over the daemon's JSON-RPC link
//! instead of connecting to DBus directly.

use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ImageEntry, MachineProperties, StatusUpdate};
use crate::nspawn::sys::daemon::ElevatedDaemon;
use std::sync::Arc;

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
        self.call("dbus_start", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.call("dbus_terminate", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.call("dbus_poweroff", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.call("dbus_reboot", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.call("dbus_enable", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.call("dbus_disable", serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }

    async fn kill(&self, name: &str, signal: crate::nspawn::models::AllowedSignal) -> Result<()> {
        self.call(
            "dbus_kill",
            serde_json::json!({"name": name, "signal": signal}),
        )
        .await?;
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.call("dbus_remove", serde_json::json!({"name": name}))
            .await?;
        Ok(())
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
        self.call("dbus_reload_daemon", serde_json::json!({}))
            .await?;
        Ok(())
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
                    break;
                }
            }
        }
        Ok(())
    }
}
