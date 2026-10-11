//! Work requested by a wizard and scoped to its lifetime.
//!
//! The application transports these events and applies overlay effects. The
//! wizard owns the requests, task handles and acceptance of their results.

use crate::application::provisioning::{
    DeploymentError, DeploymentId, DeploymentPreflight, DeploymentRequest, HostHardwareSnapshot,
    InterfaceValidation, ProvisioningHostSnapshot, ProvisioningPreparationService,
    ProvisioningService,
};
use crate::application::HostOperationTracker;
use crate::composition::PermissionLevel;
use crate::config::AppConfig;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::tui::features::wizard::core::draft::WizardDraft;
use crate::tui::features::wizard::dialogs::bootstrap_warning::BootstrapWarningDialog;
use crate::tui::features::wizard::{Wizard, WizardInstanceId, WizardSession};
use crate::tui::foundation::status::StatusLevel;
use crate::tui::protocol::events::AppEvent;
use crate::tui::protocol::Component;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub(crate) struct WizardServices {
    pub preparation: Arc<ProvisioningPreparationService>,
    pub provisioning: Arc<ProvisioningService>,
    pub host_operations: HostOperationTracker,
}

pub(crate) struct WizardLaunch {
    pub id: WizardInstanceId,
    pub machines: Vec<MachineEntry>,
    pub images: Vec<ImageEntry>,
    pub permissions: PermissionLevel,
    pub config: Arc<AppConfig>,
}

/// Opening is cancellable too: host inspection never blocks input dispatch.
pub(crate) struct WizardOpening {
    launch: Option<WizardLaunch>,
    services: WizardServices,
    task: JoinHandle<()>,
}

pub(crate) struct WizardOpened {
    pub session: WizardSession,
    pub effect: WizardEffect,
}

impl WizardOpening {
    pub fn start(launch: WizardLaunch, services: WizardServices, tx: Sender<AppEvent>) -> Self {
        let id = launch.id;
        let preparation = services.preparation.clone();
        let task = tokio::spawn(async move {
            let result =
                contain_task("provisioning host inspection", preparation.inspect_host()).await;
            let _ = tx
                .send(AppEvent::Wizard(WizardEvent::Opened { id, result }))
                .await;
        });
        Self {
            launch: Some(launch),
            services,
            task,
        }
    }

    pub fn id(&self) -> WizardInstanceId {
        self.launch
            .as_ref()
            .expect("opening retains its launch context")
            .id
    }

    pub async fn shutdown(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }

    pub fn finish(
        mut self,
        result: Result<ProvisioningHostSnapshot, DeploymentError>,
        tx: Option<Sender<AppEvent>>,
    ) -> Result<WizardOpened, DeploymentError> {
        let host = result?;
        let launch = self.launch.take().expect("opening completes once");
        let mut bootstrap = launch.config.bootstrap.resolve();
        let effect =
            BootstrapWarningDialog::from_diagnostics(std::mem::take(&mut bootstrap.diagnostics))
                .map(|dialog| WizardEffect::OpenDialog(Box::new(dialog)))
                .unwrap_or(WizardEffect::None);
        let draft = WizardDraft::new(
            launch.machines,
            launch.images,
            launch.config.nvidia.source(),
            host,
            bootstrap,
        );
        let wizard = Wizard::new(
            launch.id,
            draft,
            launch.permissions,
            self.services.preparation.clone(),
        );
        let mut session = WizardSession::new(wizard);
        session.discover_hardware(&self.services, tx);
        Ok(WizardOpened { session, effect })
    }
}

impl Drop for WizardOpening {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub(crate) enum WizardEvent {
    Opened {
        id: WizardInstanceId,
        result: Result<ProvisioningHostSnapshot, DeploymentError>,
    },
    Completed {
        id: WizardInstanceId,
        completion: WizardCompletion,
    },
}

#[derive(Debug)]
pub(crate) enum WizardCompletion {
    Hardware(Result<HostHardwareSnapshot, DeploymentError>),
    Interface {
        query: u64,
        result: Result<InterfaceValidation, DeploymentError>,
    },
    Preflight {
        query: u64,
        request: Box<DeploymentRequest>,
        result: Result<DeploymentPreflight, DeploymentError>,
    },
    ClaimRelease {
        deployment_id: DeploymentId,
        result: Result<(), DeploymentError>,
    },
}

impl WizardEvent {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "wizard-opened",
            Self::Completed { completion, .. } => match completion {
                WizardCompletion::Hardware(_) => "wizard-hardware-discovery",
                WizardCompletion::Interface { .. } => "wizard-interface-validation",
                WizardCompletion::Preflight { .. } => "deployment-preflight",
                WizardCompletion::ClaimRelease { .. } => "deployment-claim-release",
            },
        }
    }
}

pub(crate) enum WizardEffect {
    None,
    Close,
    Status(String, StatusLevel),
    OpenDialog(Box<dyn Component>),
    CloseDialog,
}

pub(crate) async fn contain_task<T>(
    label: &'static str,
    future: impl std::future::Future<Output = Result<T, DeploymentError>>,
) -> Result<T, DeploymentError> {
    use futures_util::FutureExt as _;

    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            let message = format!("{label} panicked: {detail}");
            log::error!("{message}");
            Err(DeploymentError::failed(message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn task_panics_are_returned_as_typed_failures() {
        let result: Result<(), DeploymentError> = contain_task("hardware discovery", async {
            panic!("panic sentinel");
        })
        .await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("hardware discovery panicked"));
        assert!(error.to_string().contains("panic sentinel"));
    }
}
