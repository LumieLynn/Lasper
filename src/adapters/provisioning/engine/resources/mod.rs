//! Resource ownership and cleanup blockers recorded during one deployment.

use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::application::provisioning::{
    DeploymentResource, ResourceApplyStatus, ResourceDisposition, ResourceLedger,
};

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
        status: ResourceApplyStatus,
    ) -> OperationResult<()> {
        match status {
            ResourceApplyStatus::Created => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Created);
                Ok(())
            }
            ResourceApplyStatus::Unchanged => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an unchanged .nspawn configuration predates this deployment".into());
                }
                Ok(())
            }
            ResourceApplyStatus::ReplacedOwned => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Adopted);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("a replaced .nspawn configuration cannot be restored".into());
                    return Err(OperationFailure::ResourceConflict(format!(
                        "Deployment cannot roll back replaced {} without its previous content",
                        resource.label()
                    )));
                }
                // A create intent may adopt and replace a stale, proven-owned
                // sidecar. Rollback removes the replacement instead of
                // restoring stale state for a target that was not deployed.
                Ok(())
            }
            ResourceApplyStatus::ConflictUnknownOwner => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an existing .nspawn configuration has unknown ownership".into());
                    return Err(OperationFailure::ResourceConflict(format!(
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
            DeploymentResource::RootfsHostname(self.target.clone()),
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

#[cfg(test)]
mod tests;
