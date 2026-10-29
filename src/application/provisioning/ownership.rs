//! Resource ownership evidence shared by execution, rollback and recovery.

use crate::domain::machine::MachineName;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub(crate) enum DeploymentResource {
    LocalStorage(MachineName),
    ExternalImage(MachineName),
    StorageMount(MachineName),
    RawConfigurationMount(MachineName),
    NvidiaState(MachineName),
    NspawnConfig(MachineName),
    SystemdOverride(MachineName),
    RootfsHostname(MachineName),
    RootfsAccounts(MachineName),
    RootfsNvidia(MachineName),
    RootfsNetwork(MachineName),
}

impl DeploymentResource {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::LocalStorage(_) => "local storage",
            Self::ExternalImage(_) => "external image",
            Self::StorageMount(_) => "storage mount",
            Self::RawConfigurationMount(_) => "raw image configuration mount",
            Self::NvidiaState(_) => "NVIDIA state",
            Self::NspawnConfig(_) => ".nspawn configuration",
            Self::SystemdOverride(_) => "systemd service override",
            Self::RootfsHostname(_) => "rootfs hostname state",
            Self::RootfsAccounts(_) => "rootfs account state",
            Self::RootfsNvidia(_) => "rootfs NVIDIA state",
            Self::RootfsNetwork(_) => "rootfs network state",
        }
    }

    pub(super) fn target(&self) -> &MachineName {
        match self {
            Self::LocalStorage(target)
            | Self::ExternalImage(target)
            | Self::StorageMount(target)
            | Self::RawConfigurationMount(target)
            | Self::NvidiaState(target)
            | Self::NspawnConfig(target)
            | Self::SystemdOverride(target)
            | Self::RootfsHostname(target)
            | Self::RootfsAccounts(target)
            | Self::RootfsNvidia(target)
            | Self::RootfsNetwork(target) => target,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceDisposition {
    Created,
    Adopted,
    PreExisting,
    Transferred,
    Committed,
    CleanupPending,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceLedgerEntry {
    pub(crate) resource: DeploymentResource,
    pub(crate) disposition: ResourceDisposition,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResourceLedger {
    entries: Vec<ResourceLedgerEntry>,
}

impl ResourceLedger {
    pub(crate) fn record(
        &mut self,
        resource: DeploymentResource,
        disposition: ResourceDisposition,
    ) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.resource == resource)
        {
            entry.disposition = disposition;
        } else {
            self.entries.push(ResourceLedgerEntry {
                resource,
                disposition,
            });
        }
    }

    pub(crate) fn owns(&self, resource: &DeploymentResource) -> bool {
        self.entries.iter().any(|entry| {
            &entry.resource == resource
                && matches!(
                    entry.disposition,
                    ResourceDisposition::Created
                        | ResourceDisposition::Adopted
                        | ResourceDisposition::Transferred
                        | ResourceDisposition::Committed
                        | ResourceDisposition::CleanupPending
                )
        })
    }

    pub(crate) fn disposition(&self, resource: &DeploymentResource) -> Option<ResourceDisposition> {
        self.entries
            .iter()
            .find(|entry| &entry.resource == resource)
            .map(|entry| entry.disposition)
    }

    pub(crate) fn remove(&mut self, resource: &DeploymentResource) {
        self.entries.retain(|entry| &entry.resource != resource);
    }

    pub(crate) fn owned_in_reverse(&self) -> Vec<DeploymentResource> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| self.owns(&entry.resource))
            .map(|entry| entry.resource.clone())
            .collect()
    }

    pub(crate) fn snapshot(&self) -> ResourceLedgerSnapshot {
        ResourceLedgerSnapshot {
            entries: self.entries.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceLedgerSnapshot {
    entries: Vec<ResourceLedgerEntry>,
}

impl ResourceLedgerSnapshot {
    pub(crate) fn entries(&self) -> &[ResourceLedgerEntry] {
        &self.entries
    }

    pub(super) fn validate(&self, target: &MachineName) -> Result<(), &'static str> {
        let mut resources = std::collections::HashSet::new();
        if self
            .entries
            .iter()
            .any(|entry| !resources.insert(&entry.resource))
        {
            return Err("deployment ledger contains a duplicate resource");
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.resource.target() != target)
        {
            return Err("deployment ledger contains a resource for another target");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_ledger_updates_one_typed_resource_without_duplicates() {
        let machine = MachineName::new("test").unwrap();
        let resource = DeploymentResource::NspawnConfig(machine);
        let mut ledger = ResourceLedger::default();
        ledger.record(resource.clone(), ResourceDisposition::Created);
        ledger.record(resource.clone(), ResourceDisposition::CleanupPending);

        assert!(ledger.owns(&resource));
        assert_eq!(ledger.snapshot().entries.len(), 1);
        assert_eq!(
            ledger.snapshot().entries[0].disposition,
            ResourceDisposition::CleanupPending
        );
    }

    #[test]
    fn outcome_unknown_is_recovery_evidence_not_ownership() {
        let machine = MachineName::new("test").unwrap();
        let resource = DeploymentResource::NspawnConfig(machine);
        let mut ledger = ResourceLedger::default();
        ledger.record(resource.clone(), ResourceDisposition::OutcomeUnknown);

        assert!(!ledger.owns(&resource));
        assert_eq!(
            ledger.disposition(&resource),
            Some(ResourceDisposition::OutcomeUnknown)
        );
    }
}
