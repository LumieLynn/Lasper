//! Application-owned runtime discovery and observation.

mod error;

pub use error::{RuntimeError, RuntimeResult};

use super::operations::{ExecutionRoute, RouteFallback};
use crate::domain::inspection::{MachineProperties, GROUP_MACHINE, GROUP_SYSTEMD_UNIT};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, RuntimeSnapshot, StatusUpdate};
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeQuery<T> {
    pub value: T,
    pub route: ExecutionRoute,
    pub fallback: Option<RouteFallback>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeUpdate {
    Snapshot(RuntimeQuery<RuntimeSnapshot>),
    BackendFailure {
        message: String,
        consecutive_failures: u32,
    },
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(crate) trait RuntimePort: Send + Sync + 'static {
    fn snapshot_route(&self) -> ExecutionRoute;
    fn inspection_route(&self) -> ExecutionRoute;
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>>;
    async fn snapshot(&self) -> RuntimeResult<RuntimeSnapshot>;
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> RuntimeResult<MachineProperties>;
    async fn watch(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> RuntimeResult<()>;
}

pub struct RuntimeCatalog {
    primary: Option<Arc<dyn RuntimePort>>,
    fallback: Arc<dyn RuntimePort>,
    watch_paths: Vec<PathBuf>,
    invalidation_tx: tokio::sync::broadcast::Sender<()>,
    poll_nudge_tx: Option<tokio::sync::watch::Sender<()>>,
}

impl RuntimeCatalog {
    pub(crate) fn new(
        primary: Option<Arc<dyn RuntimePort>>,
        fallback: Arc<dyn RuntimePort>,
        watch_paths: Vec<PathBuf>,
        poll_nudge_tx: Option<tokio::sync::watch::Sender<()>>,
    ) -> Self {
        let (invalidation_tx, _) = tokio::sync::broadcast::channel(16);
        Self {
            primary,
            fallback,
            watch_paths,
            invalidation_tx,
            poll_nudge_tx,
        }
    }

    pub fn invalidate(&self) {
        let _ = self.invalidation_tx.send(());
        if let Some(tx) = &self.poll_nudge_tx {
            let _ = tx.send(());
        }
    }

    pub async fn machines(&self) -> RuntimeResult<RuntimeQuery<Vec<MachineEntry>>> {
        if let Some(primary) = &self.primary {
            if primary.is_available().await {
                match primary.list_machines().await {
                    Ok(value) => {
                        return Ok(RuntimeQuery {
                            value,
                            route: primary.snapshot_route(),
                            fallback: None,
                        })
                    }
                    Err(error) => {
                        return self
                            .fallback_machines(primary.snapshot_route(), error)
                            .await
                    }
                }
            }
            return self
                .fallback_machines(
                    primary.snapshot_route(),
                    RuntimeError::unavailable("D-Bus not available"),
                )
                .await;
        }

        Ok(RuntimeQuery {
            value: self.fallback.list_machines().await?,
            route: self.fallback.snapshot_route(),
            fallback: None,
        })
    }

    pub async fn snapshot(&self) -> RuntimeResult<RuntimeQuery<RuntimeSnapshot>> {
        if let Some(primary) = &self.primary {
            if primary.is_available().await {
                match primary.snapshot().await {
                    Ok(value) => {
                        return Ok(RuntimeQuery {
                            value,
                            route: primary.snapshot_route(),
                            fallback: None,
                        })
                    }
                    Err(error) => {
                        return self
                            .fallback_snapshot(primary.snapshot_route(), error)
                            .await
                    }
                }
            }
            return self
                .fallback_snapshot(
                    primary.snapshot_route(),
                    RuntimeError::unavailable("D-Bus not available"),
                )
                .await;
        }

        Ok(RuntimeQuery {
            value: self.fallback.snapshot().await?,
            route: self.fallback.snapshot_route(),
            fallback: None,
        })
    }

    pub async fn inspect(
        &self,
        name: &str,
        entry: &MachineEntry,
    ) -> RuntimeResult<RuntimeQuery<MachineProperties>> {
        let machine = MachineName::new(name)
            .map_err(|error| RuntimeError::invalid_input(error.to_string()))?;
        let mut query = if let Some(primary) = &self.primary {
            if primary.is_available().await {
                match primary.inspect(&machine, entry).await {
                    Ok(value) => RuntimeQuery {
                        value,
                        route: primary.inspection_route(),
                        fallback: None,
                    },
                    Err(error) => {
                        self.fallback_inspection(&machine, entry, primary.inspection_route(), error)
                            .await?
                    }
                }
            } else {
                self.fallback_inspection(
                    &machine,
                    entry,
                    primary.inspection_route(),
                    RuntimeError::unavailable("D-Bus not available"),
                )
                .await?
            }
        } else {
            RuntimeQuery {
                value: self.fallback.inspect(&machine, entry).await?,
                route: self.fallback.inspection_route(),
                fallback: None,
            }
        };

        enrich_properties(&mut query.value, entry);
        Ok(query)
    }

    pub async fn watch(self: &Arc<Self>, tx: tokio::sync::mpsc::Sender<RuntimeUpdate>) {
        let primary_available = match &self.primary {
            Some(primary) => primary.is_available().await,
            None => false,
        };
        let fallback_active = Arc::new(AtomicBool::new(!primary_available));
        let fallback_reason = Arc::new(parking_lot::Mutex::new(None::<String>));
        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::channel::<StatusUpdate>(8);

        if primary_available {
            let primary = self.primary.as_ref().expect("available primary").clone();
            let fallback = self.fallback.clone();
            let primary_tx = raw_tx.clone();
            let fallback_tx = raw_tx.clone();
            let fallback_active_for_task = fallback_active.clone();
            let fallback_reason_for_task = fallback_reason.clone();
            tokio::spawn(async move {
                if let Err(error) = primary.watch(primary_tx).await {
                    log::warn!(
                        "D-Bus runtime observer unavailable ({}), falling back to systemd tools polling",
                        error
                    );
                    *fallback_reason_for_task.lock() = Some(error.to_string());
                    fallback_active_for_task.store(true, Ordering::Release);
                    if let Err(fallback_error) = fallback.watch(fallback_tx).await {
                        log::error!("systemd tools runtime observer stopped: {}", fallback_error);
                    }
                }
            });
        } else {
            let fallback = self.fallback.clone();
            let fallback_tx = raw_tx.clone();
            tokio::spawn(async move {
                if let Err(error) = fallback.watch(fallback_tx).await {
                    log::error!("systemd tools runtime observer stopped: {}", error);
                }
            });
        }

        spawn_filesystem_observer(self.watch_paths.clone(), Arc::clone(self));
        spawn_heartbeat(Arc::clone(self), fallback_active.clone());

        let catalog = Arc::clone(self);
        let mut invalidations = self.invalidation_tx.subscribe();
        tokio::spawn(async move {
            let mut dirty_failures = 0u32;
            loop {
                let raw = tokio::select! {
                    update = raw_rx.recv() => match update {
                        Some(update) => Some(update),
                        None => break,
                    },
                    invalidated = invalidations.recv() => match invalidated {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            Some(StatusUpdate::Dirty)
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                };

                let update = match raw.expect("selected runtime update") {
                    StatusUpdate::Dirty => match catalog.snapshot().await {
                        Ok(snapshot) => {
                            dirty_failures = 0;
                            RuntimeUpdate::Snapshot(snapshot)
                        }
                        Err(error) => {
                            dirty_failures = dirty_failures.saturating_add(1);
                            RuntimeUpdate::BackendFailure {
                                message: error.to_string(),
                                consecutive_failures: dirty_failures,
                            }
                        }
                    },
                    StatusUpdate::Snapshot(snapshot) => {
                        dirty_failures = 0;
                        let route = catalog.fallback.snapshot_route();
                        let fallback = catalog.primary.as_ref().map(|primary| RouteFallback {
                            from: primary.snapshot_route(),
                            to: route,
                            reason: fallback_reason
                                .lock()
                                .clone()
                                .unwrap_or_else(|| "D-Bus observer unavailable".into()),
                        });
                        RuntimeUpdate::Snapshot(RuntimeQuery {
                            value: snapshot,
                            route,
                            fallback,
                        })
                    }
                    StatusUpdate::BackendFailure {
                        message,
                        consecutive_failures,
                    } => RuntimeUpdate::BackendFailure {
                        message,
                        consecutive_failures,
                    },
                };
                if tx.send(update).await.is_err() {
                    break;
                }
            }
        });

        self.invalidate();
    }

    async fn fallback_machines(
        &self,
        from: ExecutionRoute,
        error: RuntimeError,
    ) -> RuntimeResult<RuntimeQuery<Vec<MachineEntry>>> {
        let to = self.fallback.snapshot_route();
        let reason = fallback_reason(&error);
        log::warn!(
            "{} runtime query failed ({}), using {}",
            from.label(),
            reason,
            to.label()
        );
        Ok(RuntimeQuery {
            value: self.fallback.list_machines().await?,
            route: to,
            fallback: Some(RouteFallback { from, to, reason }),
        })
    }

    async fn fallback_snapshot(
        &self,
        from: ExecutionRoute,
        error: RuntimeError,
    ) -> RuntimeResult<RuntimeQuery<RuntimeSnapshot>> {
        let to = self.fallback.snapshot_route();
        let reason = fallback_reason(&error);
        log::warn!(
            "{} snapshot failed ({}), using {}",
            from.label(),
            reason,
            to.label()
        );
        Ok(RuntimeQuery {
            value: self.fallback.snapshot().await?,
            route: to,
            fallback: Some(RouteFallback { from, to, reason }),
        })
    }

    async fn fallback_inspection(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
        from: ExecutionRoute,
        error: RuntimeError,
    ) -> RuntimeResult<RuntimeQuery<MachineProperties>> {
        let to = self.fallback.inspection_route();
        let reason = fallback_reason(&error);
        log::warn!(
            "{} inspection failed for {} ({}), using {}",
            from.label(),
            machine,
            reason,
            to.label()
        );
        Ok(RuntimeQuery {
            value: self.fallback.inspect(machine, entry).await?,
            route: to,
            fallback: Some(RouteFallback { from, to, reason }),
        })
    }
}

fn fallback_reason(error: &RuntimeError) -> String {
    if error.is_permission_denied() {
        "polkit denied access; run with -e to elevate".into()
    } else {
        error.to_string()
    }
}

fn enrich_properties(properties: &mut MachineProperties, entry: &MachineEntry) {
    properties.insert(GROUP_MACHINE, "Class".into(), entry.class.to_string());
    properties.insert(GROUP_MACHINE, "Service".into(), entry.service.to_string());
    properties.insert(
        GROUP_MACHINE,
        "IPAddresses".into(),
        entry.addresses.property_value(),
    );
    if let Some(unit_file_state) = properties
        .get_group(GROUP_SYSTEMD_UNIT)
        .and_then(|group| group.get("UnitFileState"))
        .cloned()
    {
        properties.insert(GROUP_SYSTEMD_UNIT, "Enabled".into(), unit_file_state);
    }
}

fn spawn_filesystem_observer(paths: Vec<PathBuf>, catalog: Arc<RuntimeCatalog>) {
    tokio::spawn(async move {
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = match RecommendedWatcher::new(
            move |result: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    if matches!(
                        event.kind,
                        notify::EventKind::Create(_)
                            | notify::EventKind::Remove(_)
                            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                    ) {
                        let _ = notify_tx.send(());
                    }
                }
            },
            Config::default(),
        ) {
            Ok(watcher) => watcher,
            Err(error) => {
                log::error!(
                    "Failed to create runtime filesystem watcher: {}; relying on backend observation",
                    error
                );
                return;
            }
        };

        for path in paths {
            if path.exists() {
                if let Err(error) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                    log::warn!(
                        "Runtime filesystem watcher unavailable for {} ({}); relying on backend observation",
                        path.display(),
                        error
                    );
                }
            }
        }

        while notify_rx.recv().await.is_some() {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            while notify_rx.try_recv().is_ok() {}
            catalog.invalidate();
        }
    });
}

fn spawn_heartbeat(catalog: Arc<RuntimeCatalog>, fallback_active: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));
        interval.tick().await;
        loop {
            interval.tick().await;
            if !fallback_active.load(Ordering::Acquire) {
                catalog.invalidate();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{ImageEntry, MachineState};

    fn entry(name: &str) -> MachineEntry {
        MachineEntry {
            name: name.into(),
            class: MachineEntry::NSPAWN_CLASS.into(),
            service: MachineEntry::NSPAWN_SERVICE.into(),
            state: MachineState::Running,
            addresses: Default::default(),
        }
    }

    fn snapshot(name: &str) -> RuntimeSnapshot {
        RuntimeSnapshot::new(
            vec![entry(name)],
            vec![ImageEntry {
                name: name.into(),
                image_type: "directory".into(),
                readonly: false,
                usage: None,
                dbus_object_path: None,
            }],
        )
    }

    fn port(route: ExecutionRoute) -> MockRuntimePort {
        let mut port = MockRuntimePort::new();
        port.expect_snapshot_route().return_const(route);
        port.expect_inspection_route().return_const(route);
        port
    }

    #[tokio::test]
    async fn snapshot_uses_available_primary_without_fallback() {
        let mut primary = port(ExecutionRoute::DirectDbus);
        primary.expect_is_available().returning(|| true);
        primary
            .expect_snapshot()
            .returning(|| Ok(snapshot("primary")));
        let fallback = port(ExecutionRoute::LocalSystemdTools);
        let catalog =
            RuntimeCatalog::new(Some(Arc::new(primary)), Arc::new(fallback), vec![], None);

        let query = catalog.snapshot().await.unwrap();

        assert_eq!(query.value.machines[0].name, "primary");
        assert_eq!(query.route, ExecutionRoute::DirectDbus);
        assert!(query.fallback.is_none());
    }

    #[tokio::test]
    async fn snapshot_falls_back_as_one_normalized_query() {
        let mut primary = port(ExecutionRoute::DirectDbus);
        primary.expect_is_available().returning(|| true);
        primary
            .expect_snapshot()
            .returning(|| Err(RuntimeError::failed("primary failed")));
        let mut fallback = port(ExecutionRoute::LocalSystemdTools);
        fallback
            .expect_snapshot()
            .returning(|| Ok(snapshot("fallback")));
        let catalog =
            RuntimeCatalog::new(Some(Arc::new(primary)), Arc::new(fallback), vec![], None);

        let query = catalog.snapshot().await.unwrap();

        assert_eq!(query.value.machines[0].name, "fallback");
        assert_eq!(query.route, ExecutionRoute::LocalSystemdTools);
        assert_eq!(query.fallback.unwrap().from, ExecutionRoute::DirectDbus);
    }

    #[tokio::test]
    async fn systemd_tools_only_catalog_never_probes_a_primary() {
        let mut fallback = port(ExecutionRoute::LocalSystemdTools);
        fallback
            .expect_snapshot()
            .returning(|| Ok(snapshot("systemd-tools")));
        let catalog = RuntimeCatalog::new(None, Arc::new(fallback), vec![], None);

        let query = catalog.snapshot().await.unwrap();

        assert_eq!(query.route, ExecutionRoute::LocalSystemdTools);
        assert!(query.fallback.is_none());
    }

    #[tokio::test]
    async fn inspection_enriches_route_output_from_the_snapshot_entry() {
        let mut fallback = port(ExecutionRoute::LocalSystemdTools);
        fallback.expect_inspect().returning(|_, _| {
            let mut properties = MachineProperties::default();
            properties.insert(GROUP_SYSTEMD_UNIT, "UnitFileState".into(), "enabled".into());
            Ok(properties)
        });
        let catalog = RuntimeCatalog::new(None, Arc::new(fallback), vec![], None);
        let mut machine = entry("test");
        machine.addresses = crate::domain::runtime::MachineAddressObservation::available([
            "10.0.0.2".into(),
            "fd00::2".into(),
        ]);

        let query = catalog.inspect("test", &machine).await.unwrap();

        assert_eq!(
            query
                .value
                .get_group(GROUP_MACHINE)
                .and_then(|group| group.get("IPAddresses"))
                .map(String::as_str),
            Some("10.0.0.2, fd00::2")
        );
        assert_eq!(
            query
                .value
                .get_group(GROUP_SYSTEMD_UNIT)
                .and_then(|group| group.get("Enabled"))
                .map(String::as_str),
            Some("enabled")
        );
    }
}
