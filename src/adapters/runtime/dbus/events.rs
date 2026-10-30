//! Runtime invalidations from machined registration signals.

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::runtime::map_runtime_error;
use crate::application::runtime::{RuntimeChangePort, RuntimeResult};

use super::DbusBackend;

#[async_trait::async_trait]
impl RuntimeChangePort for DbusBackend {
    async fn watch_changes(&self, changed: tokio::sync::watch::Sender<()>) -> RuntimeResult<()> {
        self.watch_events(changed).await.map_err(map_runtime_error)
    }
}

impl DbusBackend {
    pub(crate) async fn watch_events(&self, changed: tokio::sync::watch::Sender<()>) -> Result<()> {
        use futures_util::StreamExt;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;

        let new_result = self
            .query_with_deadline(
                generation,
                "machine-new subscription",
                proxy.receive_machine_new(),
            )
            .await;
        let mut new_stream = new_result.map_err(NspawnError::Dbus)?;
        let removed_result = self
            .query_with_deadline(
                generation,
                "machine-removed subscription",
                proxy.receive_machine_removed(),
            )
            .await;
        let mut rm_stream = removed_result.map_err(NspawnError::Dbus)?;

        loop {
            tokio::select! {
                event = new_stream.next() => {
                    if event.is_none() {
                        self.invalidate_connection(generation).await;
                        return Err(NspawnError::Dbus(zbus::Error::Failure(
                            "machine-new signal stream closed".into(),
                        )));
                    }
                    if changed.send(()).is_err() {
                        return Ok(());
                    }
                }
                event = rm_stream.next() => {
                    if event.is_none() {
                        self.invalidate_connection(generation).await;
                        return Err(NspawnError::Dbus(zbus::Error::Failure(
                            "machine-removed signal stream closed".into(),
                        )));
                    }
                    if changed.send(()).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}
