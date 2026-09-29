//! Configuration inspection and editing over one composition-selected route.

mod document;
mod edit;
mod execution;
mod inspection;
mod patch;
mod projection;
mod write;

pub(crate) use execution::execute;

use crate::adapters::elevated::ElevatedDaemon;
use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationError, ConfigurationPort,
    ConfigurationPreview, ConfigurationSnapshot, ConfigurationTarget,
};
use crate::ipc::protocol::configuration::{
    ConfigurationOperation, ConfigurationResult, ConfigurationValue,
};
use std::sync::Arc;

pub(crate) struct ConfigurationAdapter {
    route: ConfigurationRoute,
}

enum ConfigurationRoute {
    Direct,
    Elevated(Arc<ElevatedDaemon>),
}

impl ConfigurationAdapter {
    pub(crate) fn direct() -> Self {
        Self {
            route: ConfigurationRoute::Direct,
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            route: ConfigurationRoute::Elevated(daemon),
        }
    }

    async fn execute(&self, operation: ConfigurationOperation) -> ConfigurationResult {
        match &self.route {
            ConfigurationRoute::Direct => execute(operation).await,
            ConfigurationRoute::Elevated(daemon) => daemon.configuration(operation).await,
        }
    }
}

#[async_trait::async_trait]
impl ConfigurationPort for ConfigurationAdapter {
    async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        match self
            .execute(ConfigurationOperation::Inspect(target.clone()))
            .await?
        {
            ConfigurationValue::Inspection(snapshot) => Ok(*snapshot),
            _ => Err(ConfigurationError::failed(
                "Configuration inspection returned an unexpected response",
            )),
        }
    }

    async fn preview(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationPreview, ConfigurationError> {
        match self
            .execute(ConfigurationOperation::Preview(Box::new(edit.clone())))
            .await?
        {
            ConfigurationValue::Preview(preview) => Ok(preview),
            _ => Err(ConfigurationError::failed(
                "Configuration preview returned an unexpected response",
            )),
        }
    }

    async fn apply(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationApplyReport, ConfigurationError> {
        match self
            .execute(ConfigurationOperation::Apply(Box::new(edit.clone())))
            .await?
        {
            ConfigurationValue::Apply(report) => Ok(report),
            _ => Err(ConfigurationError::outcome_unknown(
                "Configuration save returned an unexpected response",
            )),
        }
    }
}
