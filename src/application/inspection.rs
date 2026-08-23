//! Semantic read models used by resource detail consumers.

use crate::nspawn::models::MachineProperties;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NspawnConfigInspection {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdDropInInspection {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemdUnitInspection {
    pub unit: String,
    pub drop_ins: Vec<SystemdDropInInspection>,
}

#[derive(Clone, Debug)]
pub struct ImageUnitInspection {
    pub properties: Result<Option<MachineProperties>, ResourceInspectionError>,
    pub unit: Option<Result<SystemdUnitInspection, ResourceInspectionError>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ResourceInspectionError {
    message: String,
}

impl ResourceInspectionError {
    pub(crate) fn backend(error: impl std::fmt::Display) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait ResourceInspectionPort: Send + Sync + 'static {
    async fn inspect_nspawn_config(
        &self,
        name: &str,
    ) -> Result<Option<NspawnConfigInspection>, ResourceInspectionError>;

    async fn inspect_image_unit(&self, name: &str) -> ImageUnitInspection;
}

pub struct ResourceInspectionService {
    port: Arc<dyn ResourceInspectionPort>,
}

impl ResourceInspectionService {
    pub(crate) fn new(port: Arc<dyn ResourceInspectionPort>) -> Self {
        Self { port }
    }

    pub async fn inspect_nspawn_config(
        &self,
        name: &str,
    ) -> Result<Option<NspawnConfigInspection>, ResourceInspectionError> {
        self.port.inspect_nspawn_config(name).await
    }

    pub async fn inspect_image_unit(&self, name: &str) -> ImageUnitInspection {
        self.port.inspect_image_unit(name).await
    }
}
