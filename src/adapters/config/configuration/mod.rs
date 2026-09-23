//! Configure capability adapter. The store selects direct/elevated execution;
//! inspection and declaration projection stay inside the configuration adapter.

mod document;
mod edit;
mod inspection;
mod patch;
mod projection;

pub(super) use edit::{apply, preview};
pub(super) use inspection::inspect;

use super::NspawnConfigStore;
use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationPort, ConfigurationPreview,
    ConfigurationSnapshot, ConfigurationTarget,
};
use crate::application::inspection::ResourceInspectionError;

pub(crate) struct StoreConfiguration {
    store: NspawnConfigStore,
}

impl StoreConfiguration {
    pub(crate) fn new(store: NspawnConfigStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ConfigurationPort for StoreConfiguration {
    async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ResourceInspectionError> {
        self.store
            .configuration_snapshot(target.clone())
            .await
            .map_err(ResourceInspectionError::backend)
    }

    async fn preview(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationPreview, ResourceInspectionError> {
        self.store
            .preview_configuration(edit.clone())
            .await
            .map_err(ResourceInspectionError::backend)
    }

    async fn apply(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationApplyReport, ResourceInspectionError> {
        self.store
            .apply_configuration(edit.clone())
            .await
            .map_err(ResourceInspectionError::backend)
    }
}
