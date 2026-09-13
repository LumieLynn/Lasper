//! Host-backed resource detail inspection.

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::process::CommandRunner;
use crate::application::inspection::{
    ImageUnitInspection, NspawnConfigInspection, ResourceInspectionError, ResourceInspectionPort,
    SystemdDropInInspection, SystemdUnitInspection,
};
use crate::domain::runtime::MachineEntry;
use std::sync::Arc;

pub(crate) struct StoreResourceInspection {
    local_cmd: Arc<dyn CommandRunner>,
    nspawn: NspawnConfigStore,
    systemd_unit: SystemdUnitStore,
}

impl StoreResourceInspection {
    pub(crate) fn new(
        local_cmd: Arc<dyn CommandRunner>,
        nspawn: NspawnConfigStore,
        systemd_unit: SystemdUnitStore,
    ) -> Self {
        Self {
            local_cmd,
            nspawn,
            systemd_unit,
        }
    }
}

#[async_trait::async_trait]
impl ResourceInspectionPort for StoreResourceInspection {
    async fn inspect_machine_nspawn_config(
        &self,
        machine: &MachineEntry,
    ) -> Result<Option<NspawnConfigInspection>, ResourceInspectionError> {
        if !machine.access().is_nspawn() {
            return Err(ResourceInspectionError::unsupported(format!(
                "machine '{}' is read-only and has no Lasper nspawn configuration",
                machine.name
            )));
        }
        self.nspawn
            .read(&machine.name)
            .await
            .map(|config| {
                config.map(|config| NspawnConfigInspection {
                    path: config.path,
                    content: config.content,
                })
            })
            .map_err(ResourceInspectionError::backend)
    }

    async fn inspect_image_nspawn_config(
        &self,
        name: &str,
    ) -> Result<Option<NspawnConfigInspection>, ResourceInspectionError> {
        self.nspawn
            .inspect(name)
            .await
            .map(|config| {
                config.map(|config| NspawnConfigInspection {
                    path: config.path,
                    content: config.content,
                })
            })
            .map_err(ResourceInspectionError::backend)
    }

    async fn inspect_image_unit(&self, name: &str) -> ImageUnitInspection {
        let properties =
            crate::adapters::runtime::systemd_tools::get_image_unit_properties_with_runner(
                name,
                self.local_cmd.as_ref(),
            )
            .await
            .map_err(ResourceInspectionError::backend);
        let unit = if matches!(properties, Ok(None)) {
            None
        } else {
            Some(
                self.systemd_unit
                    .read(name)
                    .await
                    .map(|inspection| SystemdUnitInspection {
                        unit: inspection.unit,
                        drop_ins: inspection
                            .drop_ins
                            .into_iter()
                            .map(|drop_in| SystemdDropInInspection {
                                path: drop_in.path,
                                content: drop_in.content,
                            })
                            .collect(),
                    })
                    .map_err(ResourceInspectionError::backend),
            )
        };
        ImageUnitInspection { properties, unit }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{MachineState, ReadOnlyReason};

    #[tokio::test]
    async fn non_machine_image_names_do_not_probe_systemd_unit_drop_ins() {
        let inspection = StoreResourceInspection::new(
            Arc::new(crate::adapters::process::DefaultCommandRunner),
            NspawnConfigStore::direct(),
            SystemdUnitStore::direct(),
        );

        let result = inspection.inspect_image_unit("Ubuntu Resolute image").await;

        assert!(matches!(result.properties, Ok(None)));
        assert!(result.unit.is_none());
    }

    #[tokio::test]
    async fn foreign_machine_cannot_enter_the_nspawn_config_reader() {
        let inspection = StoreResourceInspection::new(
            Arc::new(crate::adapters::process::DefaultCommandRunner),
            NspawnConfigStore::direct(),
            SystemdUnitStore::direct(),
        );
        let machine = MachineEntry {
            name: "guest".into(),
            class: "vm".into(),
            service: "systemd-vmspawn".into(),
            state: MachineState::Running,
            addresses: Default::default(),
        };

        let error = inspection
            .inspect_machine_nspawn_config(&machine)
            .await
            .unwrap_err();

        assert!(error.is_unsupported());
        assert!(matches!(
            machine.access(),
            crate::domain::runtime::MachineAccess::ReadOnly(ReadOnlyReason::VirtualMachine)
        ));
    }
}
