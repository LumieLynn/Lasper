//! Host-backed resource detail inspection.

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::runtime::inspection::MachineInspectionStore;
use crate::application::inspection::{
    ImageUnitInspection, NspawnConfigInspection, ResourceInspectionError, ResourceInspectionPort,
    SystemdDropInInspection, SystemdUnitInspection,
};

pub(crate) struct StoreResourceInspection {
    machine: MachineInspectionStore,
    nspawn: NspawnConfigStore,
    systemd_unit: SystemdUnitStore,
}

impl StoreResourceInspection {
    pub(crate) fn new(
        machine: MachineInspectionStore,
        nspawn: NspawnConfigStore,
        systemd_unit: SystemdUnitStore,
    ) -> Self {
        Self {
            machine,
            nspawn,
            systemd_unit,
        }
    }
}

#[async_trait::async_trait]
impl ResourceInspectionPort for StoreResourceInspection {
    async fn inspect_nspawn_config(
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
        let properties = self
            .machine
            .inspect_static(name)
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

    #[tokio::test]
    async fn non_machine_image_names_do_not_probe_systemd_unit_drop_ins() {
        let inspection = StoreResourceInspection::new(
            MachineInspectionStore::new(None),
            NspawnConfigStore::new(None),
            SystemdUnitStore::new(None),
        );

        let result = inspection.inspect_image_unit("Ubuntu Resolute image").await;

        assert!(matches!(result.properties, Ok(None)));
        assert!(result.unit.is_none());
    }
}
