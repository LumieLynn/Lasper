use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::platform::nvidia::NvidiaStateStore;
use crate::adapters::process::CommandRunner;
use crate::adapters::rootfs::RootfsStore;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::{
    DeploymentCancellation, DeploymentEvent as DeployLogEvent, DeploymentJobContext,
    DeploymentResource, DeploymentStage, ResourceDisposition, ResourceLedger,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ApplyStatus, ContainerConfig};

/// Direct host capabilities used by the provisioning implementation.
///
/// The elevated route submits the complete deployment to the daemon, whose
/// worker constructs this same direct bundle. Keeping this type route-specific
/// prevents a deployment stage from selecting a daemon transport internally.
#[derive(Clone)]
pub(crate) struct DirectProvisioningCapabilities {
    pub(super) system_operations: SystemOperationStore,
    pub(super) nspawn: NspawnConfigStore,
    pub(super) systemd_unit: SystemdUnitStore,
    pub(super) rootfs: RootfsStore,
    pub(super) nvidia_state: NvidiaStateStore,
}

impl DirectProvisioningCapabilities {
    pub(crate) fn from_direct(
        command_runner: std::sync::Arc<dyn CommandRunner>,
        state_root: TrustedStateRoot,
    ) -> Self {
        Self {
            system_operations: SystemOperationStore::direct(command_runner),
            nspawn: NspawnConfigStore::direct(),
            systemd_unit: SystemdUnitStore::direct(),
            rootfs: RootfsStore::direct(),
            nvidia_state: NvidiaStateStore::direct(state_root),
        }
    }

    pub(crate) fn system_operations(&self) -> &SystemOperationStore {
        &self.system_operations
    }

    pub(crate) fn nspawn(&self) -> &NspawnConfigStore {
        &self.nspawn
    }

    pub(crate) fn systemd_unit(&self) -> &SystemdUnitStore {
        &self.systemd_unit
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppliedResource {
    LocalStorage,
    ExternalImage,
    NvidiaState,
    NspawnConfig,
    SystemdOverride,
}

impl AppliedResource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::LocalStorage => "local storage",
            Self::ExternalImage => "external image",
            Self::NvidiaState => "NVIDIA state",
            Self::NspawnConfig => ".nspawn configuration",
            Self::SystemdOverride => "systemd service override",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ApplyReport {
    pub(super) target: crate::domain::machine::MachineName,
    pub(super) ledger: ResourceLedger,
    pub(crate) external_image_blockers: Vec<String>,
    pub(crate) storage_removal_blockers: Vec<String>,
}

impl ApplyReport {
    pub(crate) fn new(target: crate::domain::machine::MachineName) -> Self {
        Self {
            target,
            ledger: ResourceLedger::default(),
            external_image_blockers: Vec::new(),
            storage_removal_blockers: Vec::new(),
        }
    }

    fn typed(&self, resource: AppliedResource) -> DeploymentResource {
        match resource {
            AppliedResource::LocalStorage => DeploymentResource::LocalStorage(self.target.clone()),
            AppliedResource::ExternalImage => {
                DeploymentResource::ExternalImage(self.target.clone())
            }
            AppliedResource::NvidiaState => DeploymentResource::NvidiaState(self.target.clone()),
            AppliedResource::NspawnConfig => DeploymentResource::NspawnConfig(self.target.clone()),
            AppliedResource::SystemdOverride => {
                DeploymentResource::SystemdOverride(self.target.clone())
            }
        }
    }

    pub(crate) fn record_created(&mut self, resource: AppliedResource) {
        self.ledger
            .record(self.typed(resource), ResourceDisposition::Created);
    }

    pub(crate) fn record_apply(
        &mut self,
        resource: AppliedResource,
        status: ApplyStatus,
    ) -> Result<()> {
        match status {
            ApplyStatus::Created => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Created);
                Ok(())
            }
            ApplyStatus::Unchanged => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an unchanged .nspawn configuration predates this deployment".into());
                }
                Ok(())
            }
            ApplyStatus::ReplacedOwned => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Adopted);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("a replaced .nspawn configuration cannot be restored".into());
                    return Err(NspawnError::Runtime(format!(
                        "Deployment cannot roll back replaced {} without its previous content",
                        resource.label()
                    )));
                }
                // A create intent may adopt and replace a stale, proven-owned
                // sidecar. Rollback removes the replacement instead of
                // restoring stale state for a target that was not deployed.
                Ok(())
            }
            ApplyStatus::ConflictUnknownOwner => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an existing .nspawn configuration has unknown ownership".into());
                    return Err(NspawnError::Validation(format!(
                        "Refusing to replace existing {} with unknown ownership",
                        resource.label()
                    )));
                }
                // Auxiliary state is optional. Preserve the unknown file and
                // let the caller surface the degraded result as a warning.
                Ok(())
            }
        }
    }

    pub(crate) fn owns(&self, resource: AppliedResource) -> bool {
        self.ledger.owns(&self.typed(resource))
    }

    pub(crate) fn record_typed(
        &mut self,
        resource: DeploymentResource,
        disposition: ResourceDisposition,
    ) {
        self.ledger.record(resource, disposition);
    }

    pub(crate) fn record_outcome_unknown_if_unclassified(&mut self, resource: DeploymentResource) {
        if self.ledger.disposition(&resource).is_none() {
            self.ledger
                .record(resource, ResourceDisposition::OutcomeUnknown);
        }
    }

    pub(crate) fn remove_typed(&mut self, resource: &DeploymentResource) {
        self.ledger.remove(resource);
    }

    pub(crate) fn typed_owned_in_reverse(&self) -> Vec<DeploymentResource> {
        self.ledger
            .owned_in_reverse()
            .into_iter()
            .filter(|resource| {
                matches!(
                    resource,
                    DeploymentResource::LocalStorage(_)
                        | DeploymentResource::ExternalImage(_)
                        | DeploymentResource::NvidiaState(_)
                        | DeploymentResource::NspawnConfig(_)
                        | DeploymentResource::SystemdOverride(_)
                )
            })
            .collect()
    }

    pub(crate) fn block_storage_removal(&mut self, reason: impl Into<String>) {
        self.storage_removal_blockers.push(reason.into());
    }

    pub(crate) fn block_external_image_removal(&mut self, reason: impl Into<String>) {
        self.external_image_blockers.push(reason.into());
    }

    pub(crate) fn removal_blockers(&self, resource: AppliedResource) -> Vec<&str> {
        let external = if resource == AppliedResource::ExternalImage {
            self.external_image_blockers.as_slice()
        } else {
            &[]
        };
        external
            .iter()
            .chain(&self.storage_removal_blockers)
            .map(String::as_str)
            .collect()
    }

    pub(crate) fn application_ledger(&self) -> &ResourceLedger {
        &self.ledger
    }

    pub(crate) fn remove_rootfs_dependents(&mut self) {
        for resource in [
            DeploymentResource::StorageMount(self.target.clone()),
            DeploymentResource::RawConfigurationMount(self.target.clone()),
            DeploymentResource::RootfsAccounts(self.target.clone()),
            DeploymentResource::RootfsNvidia(self.target.clone()),
            DeploymentResource::RootfsNetwork(self.target.clone()),
        ] {
            self.ledger.remove(&resource);
        }
    }

    pub(crate) fn remove_external_image_dependents(&mut self) {
        self.remove_rootfs_dependents();
        self.ledger
            .remove(&DeploymentResource::NspawnConfig(self.target.clone()));
    }

    pub(crate) fn outcome_unknown_resources(&self) -> Vec<DeploymentResource> {
        self.ledger
            .snapshot()
            .entries()
            .iter()
            .filter(|entry| entry.disposition == ResourceDisposition::OutcomeUnknown)
            .map(|entry| entry.resource.clone())
            .collect()
    }
}

pub(crate) async fn persist_applying(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    intended_resources: Vec<DeploymentResource>,
    report: &ApplyReport,
) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .applying(stage, intended_resources, report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

pub(crate) async fn persist_committed(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    report: &ApplyReport,
) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .committed(stage, report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

pub(crate) async fn persist_cleanup_pending(
    job: &DeploymentJobContext,
    report: &ApplyReport,
) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .cleanup_pending(report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

pub(crate) async fn finish_manifest(job: &DeploymentJobContext) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .finish()
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

pub(crate) async fn capture_uncommitted_effects(
    job: &DeploymentJobContext,
    report: &mut ApplyReport,
) {
    let Some(state) = job.state_session() else {
        return;
    };
    for resource in state.current_applying_resources().await {
        if matches!(resource, DeploymentResource::RawConfigurationMount(_))
            && report.ledger.disposition(&resource).is_none()
        {
            report.block_storage_removal(
                "raw image configuration mount outcome requires reconciliation",
            );
        }
        report.record_outcome_unknown_if_unclassified(resource);
    }
}

#[async_trait::async_trait]
pub(crate) trait Deployer: Send + Sync {
    /// Performs the actual deployment (bootstrapping / cloning) of the container.
    async fn deploy(
        &self,
        name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()>;

    /// Returns true if this deployer manages its own storage (e.g. machinectl clone).
    fn is_external_storage_managed(&self) -> bool {
        false
    }

    /// Resources whose outcome may change while the source stage is running.
    ///
    /// This list is persisted before dispatch. It may conservatively include
    /// optional effects, but it must include every effect the deployer can
    /// create before it returns an authoritative result.
    fn source_stage_resources(
        &self,
        target: &crate::domain::machine::MachineName,
    ) -> Vec<DeploymentResource> {
        vec![if self.is_external_storage_managed() {
            DeploymentResource::ExternalImage(target.clone())
        } else {
            DeploymentResource::LocalStorage(target.clone())
        }]
    }

    /// Returns true if this deployer requires post-deployment configuration (passwords, etc).
    /// Default is true. Clones might set this to false if they are already configured.
    fn requires_post_config(&self) -> bool {
        true
    }
}
