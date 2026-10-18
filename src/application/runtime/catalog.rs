//! Normalized runtime queries and their read-only fallback policy.

use super::{RuntimeObservation, RuntimeObservers, RuntimePort, RuntimeQuery, RuntimeResult};
use crate::application::operations::{ExecutionRoute, RouteFallback};
use crate::application::runtime::RuntimeError;
use crate::domain::inspection::{MachineProperties, GROUP_MACHINE, GROUP_SYSTEMD_UNIT};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, RuntimeSnapshot};
use std::sync::Arc;

pub struct RuntimeCatalog {
    primary: Option<Arc<dyn RuntimePort>>,
    fallback: Arc<dyn RuntimePort>,
    pub(super) observers: RuntimeObservers,
    pub(super) invalidation_tx: tokio::sync::watch::Sender<()>,
}

impl RuntimeCatalog {
    pub(crate) fn new(
        primary: Option<Arc<dyn RuntimePort>>,
        fallback: Arc<dyn RuntimePort>,
        observers: RuntimeObservers,
    ) -> Self {
        let (invalidation_tx, _) = tokio::sync::watch::channel(());
        Self {
            primary,
            fallback,
            observers,
            invalidation_tx,
        }
    }

    pub fn invalidate(&self) {
        let _ = self.invalidation_tx.send(());
    }

    /// Own the refresh workflow until its consumer stops observing.
    pub fn observe(
        self: &Arc<Self>,
        tx: tokio::sync::mpsc::Sender<super::RuntimeUpdate>,
    ) -> RuntimeObservation {
        RuntimeObservation::start(Arc::clone(self), tx)
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

    /// An image can have a unit without being registered as a running machine.
    /// Filesystem-valid image names outside the machine namespace have no unit.
    pub async fn inspect_image_unit(
        &self,
        name: &str,
    ) -> RuntimeResult<Option<RuntimeQuery<MachineProperties>>> {
        let Ok(machine) = MachineName::new(name) else {
            return Ok(None);
        };
        let mut fallback = None;
        if let Some(primary) = &self.primary {
            let result = if primary.is_available().await {
                primary.inspect_unit(&machine).await
            } else {
                Err(RuntimeError::unavailable("D-Bus not available"))
            };
            match result {
                Ok(value) => {
                    return Ok(Some(RuntimeQuery {
                        value,
                        route: primary.inspection_route(),
                        fallback: None,
                    }))
                }
                Err(error) => {
                    let from = primary.inspection_route();
                    let to = self.fallback.inspection_route();
                    let reason = fallback_reason(&error);
                    log::warn!(
                        "{} unit inspection failed for {} ({}), using {}",
                        from.label(),
                        machine,
                        reason,
                        to.label()
                    );
                    fallback = Some(RouteFallback { from, to, reason });
                }
            }
        }
        Ok(Some(RuntimeQuery {
            value: self.fallback.inspect_unit(&machine).await?,
            route: self.fallback.inspection_route(),
            fallback,
        }))
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
    // Leave observed property values and their interfaces intact. These
    // derived fields only fill gaps in a partial/runtime-state inspection.
    if properties.get_display(GROUP_MACHINE, "Class").is_none() {
        properties.insert(GROUP_MACHINE, "Class".into(), entry.class.to_string());
    }
    if properties.get_display(GROUP_MACHINE, "Service").is_none() {
        properties.insert(GROUP_MACHINE, "Service".into(), entry.service.to_string());
    }
    properties.insert(
        GROUP_MACHINE,
        "IPAddresses".into(),
        entry.addresses.property_value(),
    );
    if let Some(unit_file_state) = properties
        .get_group(GROUP_SYSTEMD_UNIT)
        .and_then(|group| group.get("UnitFileState"))
        .map(|entry| entry.display.clone())
    {
        properties.insert(GROUP_SYSTEMD_UNIT, "Enabled".into(), unit_file_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::runtime::MockRuntimePort;
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
        let catalog = RuntimeCatalog::new(
            Some(Arc::new(primary)),
            Arc::new(fallback),
            Default::default(),
        );

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
        let catalog = RuntimeCatalog::new(
            Some(Arc::new(primary)),
            Arc::new(fallback),
            Default::default(),
        );

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
        let catalog = RuntimeCatalog::new(None, Arc::new(fallback), Default::default());

        let query = catalog.snapshot().await.unwrap();

        assert_eq!(query.route, ExecutionRoute::LocalSystemdTools);
        assert!(query.fallback.is_none());
    }

    #[tokio::test]
    async fn image_unit_uses_selected_dbus_route_without_machine_queries() {
        for route in [ExecutionRoute::DirectDbus, ExecutionRoute::ElevatedDbus] {
            let mut primary = port(route);
            primary.expect_is_available().once().returning(|| true);
            primary
                .expect_inspect_unit()
                .once()
                .withf(|machine| machine.as_str() == "test-image")
                .returning(|_| Ok(MachineProperties::default()));
            let catalog = RuntimeCatalog::new(
                Some(Arc::new(primary)),
                Arc::new(port(ExecutionRoute::LocalSystemdTools)),
                Default::default(),
            );
            let query = catalog
                .inspect_image_unit("test-image")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(query.route, route);
            assert!(query.fallback.is_none());
        }
    }

    #[tokio::test]
    async fn image_unit_preserves_elevated_tools_route() {
        let mut fallback = port(ExecutionRoute::ElevatedSystemdTools);
        fallback
            .expect_inspect_unit()
            .once()
            .returning(|_| Ok(MachineProperties::default()));
        let catalog = RuntimeCatalog::new(None, Arc::new(fallback), Default::default());
        let query = catalog
            .inspect_image_unit("test-image")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(query.route, ExecutionRoute::ElevatedSystemdTools);
        assert!(query.fallback.is_none());
    }

    #[tokio::test]
    async fn image_unit_query_falls_back_after_primary_failure() {
        let mut primary = port(ExecutionRoute::DirectDbus);
        primary.expect_is_available().returning(|| true);
        primary
            .expect_inspect_unit()
            .once()
            .returning(|_| Err(RuntimeError::failed("query failed")));
        let mut fallback = port(ExecutionRoute::LocalSystemdTools);
        fallback
            .expect_inspect_unit()
            .once()
            .returning(|_| Ok(MachineProperties::default()));
        let catalog = RuntimeCatalog::new(
            Some(Arc::new(primary)),
            Arc::new(fallback),
            Default::default(),
        );
        let query = catalog
            .inspect_image_unit("test-image")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(query.route, ExecutionRoute::LocalSystemdTools);
        assert_eq!(query.fallback.unwrap().from, ExecutionRoute::DirectDbus);
    }

    #[tokio::test]
    async fn inspection_enriches_route_output_from_the_snapshot_entry() {
        let mut fallback = port(ExecutionRoute::LocalSystemdTools);
        fallback.expect_inspect().returning(|_, _| {
            let mut properties = MachineProperties::default();
            properties.insert(GROUP_SYSTEMD_UNIT, "UnitFileState".into(), "enabled".into());
            Ok(properties)
        });
        let catalog = RuntimeCatalog::new(None, Arc::new(fallback), Default::default());
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
                .map(|entry| entry.display.as_str()),
            Some("10.0.0.2, fd00::2")
        );
        assert_eq!(
            query
                .value
                .get_group(GROUP_SYSTEMD_UNIT)
                .and_then(|group| group.get("Enabled"))
                .map(|entry| entry.display.as_str()),
            Some("enabled")
        );
    }
}
