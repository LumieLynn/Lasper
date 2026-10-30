//! Shared system-bus connection generations and query/mutation deadlines.

use crate::adapters::error::{NspawnError, Result};
use std::future::Future;
use std::time::Duration;
use zbus::Connection;

use super::protocol::ManagerProxy;
use super::DbusBackend;

const DBUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_MUTATION_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct ConnectionCache<T> {
    current: Option<(u64, T)>,
    next_generation: u64,
}

impl<T: Clone> ConnectionCache<T> {
    fn lease(&self) -> Option<(u64, T)> {
        self.current
            .as_ref()
            .map(|(generation, value)| (*generation, value.clone()))
    }

    pub(super) fn insert(&mut self, value: T) -> (u64, T) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.current = Some((generation, value.clone()));
        (generation, value)
    }

    fn invalidate(&mut self, generation: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            self.current = None;
        }
    }
}

impl<T> Default for ConnectionCache<T> {
    fn default() -> Self {
        Self {
            current: None,
            next_generation: 0,
        }
    }
}

impl DbusBackend {
    pub fn new() -> Self {
        Self {
            conn: std::sync::Arc::new(tokio::sync::Mutex::new(ConnectionCache::default())),
        }
    }

    pub async fn connection(&self) -> Option<Connection> {
        self.connection_lease()
            .await
            .map(|(_, connection)| connection)
    }

    pub(super) async fn connection_lease(&self) -> Option<(u64, Connection)> {
        if let Some(lease) = self.conn.lock().await.lease() {
            return Some(lease);
        }

        // Do not hold the cache mutex while waiting for a late or unavailable
        // system bus. A second cache check closes the parallel-connect race.
        let connection =
            match tokio::time::timeout(DBUS_CONNECT_TIMEOUT, Connection::system()).await {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    log::debug!("System D-Bus connection unavailable: {error}");
                    return None;
                }
                Err(_) => {
                    log::debug!(
                        "System D-Bus connection timed out after {}s",
                        DBUS_CONNECT_TIMEOUT.as_secs()
                    );
                    return None;
                }
            };
        let mut cache = self.conn.lock().await;
        Some(cache.lease().unwrap_or_else(|| cache.insert(connection)))
    }

    pub(super) async fn invalidate_connection(&self, generation: u64) {
        self.conn.lock().await.invalidate(generation);
    }

    pub(super) async fn observe_result<T>(&self, generation: u64, result: &zbus::Result<T>) {
        if result.as_ref().is_err_and(is_connection_error) {
            self.invalidate_connection(generation).await;
        }
    }

    pub(super) async fn query_with_deadline<T, F>(
        &self,
        generation: u64,
        label: &str,
        future: F,
    ) -> zbus::Result<T>
    where
        F: Future<Output = zbus::Result<T>>,
    {
        let result = match tokio::time::timeout(DBUS_QUERY_TIMEOUT, future).await {
            Ok(result) => result,
            Err(_) => {
                log::warn!(
                    "D-Bus query {label} exceeded its {}s deadline",
                    DBUS_QUERY_TIMEOUT.as_secs()
                );
                self.invalidate_connection(generation).await;
                Err(zbus::Error::Failure(format!(
                    "D-Bus query {label} timed out after {}s",
                    DBUS_QUERY_TIMEOUT.as_secs()
                )))
            }
        };
        self.observe_result(generation, &result).await;
        result
    }

    pub(super) async fn mutation_with_deadline<T, F>(
        &self,
        generation: u64,
        label: &str,
        future: F,
    ) -> Result<T>
    where
        F: Future<Output = zbus::Result<T>>,
    {
        match tokio::time::timeout(DBUS_MUTATION_TIMEOUT, future).await {
            Ok(result) => {
                self.observe_result(generation, &result).await;
                result.map_err(NspawnError::Dbus)
            }
            Err(_) => {
                log::warn!(
                    "D-Bus mutation {label} exceeded its {}s deadline; its outcome is unknown",
                    DBUS_MUTATION_TIMEOUT.as_secs()
                );
                self.invalidate_connection(generation).await;
                Err(NspawnError::SystemOperationOutcomeUnknown(format!(
                    "D-Bus mutation {label} timed out after {}s; reconcile host state before retrying",
                    DBUS_MUTATION_TIMEOUT.as_secs()
                )))
            }
        }
    }

    pub(super) async fn manager_proxy(&self) -> Option<(u64, ManagerProxy<'static>)> {
        let (generation, connection) = self.connection_lease().await?;
        let result = self
            .query_with_deadline(generation, "manager proxy", ManagerProxy::new(&connection))
            .await;
        result.ok().map(|proxy| (generation, proxy))
    }
}
pub(super) fn is_connection_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::InputOutput(_) | zbus::Error::Handshake(_) => true,
        zbus::Error::FDO(error) => match error.as_ref() {
            zbus::fdo::Error::ZBus(error) => is_connection_error(error),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_connection_failure_does_not_clear_a_new_generation() {
        let mut cache = ConnectionCache::default();
        let (stale_generation, _) = cache.insert("stale");
        let (current_generation, _) = cache.insert("current");

        cache.invalidate(stale_generation);

        assert_eq!(cache.lease(), Some((current_generation, "current")));
        cache.invalidate(current_generation);
        assert_eq!(cache.lease(), None);
    }

    #[test]
    fn backend_clones_share_one_connection_cache() {
        let backend = DbusBackend::new();
        let clone = backend.clone();

        assert!(std::sync::Arc::ptr_eq(&backend.conn, &clone.conn));
    }

    #[test]
    fn only_transport_failures_invalidate_connections() {
        let io_error = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        )));

        assert!(is_connection_error(&io_error));
        assert!(!is_connection_error(&zbus::Error::Failure(
            "service unavailable".into()
        )));
    }
}
