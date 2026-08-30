//! Secret-free deployment planning, resource ownership, and crash state.

use super::{DeploymentError, DeploymentId, DeploymentRequest};
use crate::application::{ResourceClaim, ResourceKey};
use crate::domain::machine::MachineName;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub(crate) const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct DeploymentPlan {
    request: DeploymentRequest,
    target: MachineName,
    fingerprint: PlanFingerprint,
}

impl DeploymentPlan {
    pub(crate) fn build(request: DeploymentRequest) -> Result<Self, DeploymentError> {
        request.validate()?;
        let target = crate::nspawn::models::NspawnConfigSpec::try_from(&request.config)
            .map_err(|error| DeploymentError::rejected(error.to_string()))?
            .machine;
        let normalized = serde_json::to_vec(&request).map_err(|error| {
            DeploymentError::failed(format!("Could not normalize deployment plan: {error}"))
        })?;
        let digest = Sha256::digest(&normalized);
        let fingerprint = PlanFingerprint(
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
        Ok(Self {
            request,
            target,
            fingerprint,
        })
    }

    pub fn target(&self) -> &MachineName {
        &self.target
    }

    pub(crate) fn fingerprint(&self) -> &PlanFingerprint {
        &self.fingerprint
    }

    pub(crate) fn into_request(self) -> DeploymentRequest {
        self.request
    }

    pub(crate) fn resource_claims(&self) -> Vec<ResourceClaim> {
        let mut claims = vec![ResourceClaim::exclusive(ResourceKey::for_machine(
            &self.target,
        ))];
        if let super::DeploymentSource::Copy { source_name } = &self.request.source {
            let source = crate::domain::runtime::ImageName::new(source_name.clone())
                .expect("validated deployment plan has a valid clone source");
            claims.push(ResourceClaim::shared(ResourceKey::for_image(&source)));
        }
        claims
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PlanFingerprint(String);

impl PlanFingerprint {
    pub(crate) fn is_valid(&self) -> bool {
        self.0.len() == 64 && self.0.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeploymentStage {
    Validation,
    StoragePreparation,
    SourceDeployment,
    RootfsMutation,
    HostConfiguration,
    RuntimeCommit,
    Cleanup,
    Rollback,
}

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

    fn target(&self) -> &MachineName {
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

    fn validate(&self, target: &MachineName) -> Result<(), DeploymentStateError> {
        let mut resources = std::collections::HashSet::new();
        if self
            .entries
            .iter()
            .any(|entry| !resources.insert(&entry.resource))
        {
            return Err(DeploymentStateError::Invalid(
                "deployment ledger contains a duplicate resource".into(),
            ));
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.resource.target() != target)
        {
            return Err(DeploymentStateError::Invalid(
                "deployment ledger contains a resource for another target".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DeploymentManifestState {
    Prepared,
    Applying {
        stage: DeploymentStage,
        intended_resources: Vec<DeploymentResource>,
    },
    Committed {
        stage: DeploymentStage,
    },
    CleanupPending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentCrashManifest {
    pub(crate) schema_version: u32,
    pub(crate) deployment_id: DeploymentId,
    pub(crate) target: MachineName,
    pub(crate) normalized_secret_free_plan_hash: PlanFingerprint,
    pub(crate) revision: u64,
    pub(crate) state: DeploymentManifestState,
    pub(crate) committed_ledger: ResourceLedgerSnapshot,
}

impl DeploymentCrashManifest {
    pub(crate) fn prepared(id: DeploymentId, plan: &DeploymentPlan) -> Self {
        Self {
            schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            deployment_id: id,
            target: plan.target().clone(),
            normalized_secret_free_plan_hash: plan.fingerprint().clone(),
            revision: 1,
            state: DeploymentManifestState::Prepared,
            committed_ledger: ResourceLedgerSnapshot::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), DeploymentStateError> {
        if self.schema_version != DEPLOYMENT_MANIFEST_SCHEMA_VERSION {
            return Err(DeploymentStateError::Invalid(format!(
                "unsupported deployment manifest schema {}",
                self.schema_version
            )));
        }
        if self.revision == 0 || !self.normalized_secret_free_plan_hash.is_valid() {
            return Err(DeploymentStateError::Invalid(
                "deployment manifest has an invalid revision or plan hash".into(),
            ));
        }
        self.committed_ledger.validate(&self.target)?;
        if let DeploymentManifestState::Applying {
            intended_resources, ..
        } = &self.state
        {
            let mut resources = std::collections::HashSet::new();
            if intended_resources
                .iter()
                .any(|resource| !resources.insert(resource))
            {
                return Err(DeploymentStateError::Invalid(
                    "deployment manifest contains duplicate intended resources".into(),
                ));
            }
            if intended_resources
                .iter()
                .any(|resource| resource.target() != &self.target)
            {
                return Err(DeploymentStateError::Invalid(
                    "deployment manifest intends a resource for another target".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn recovery_claim(&self) -> ResourceClaim {
        ResourceClaim::exclusive(ResourceKey::for_machine(&self.target))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub(crate) enum DeploymentStateError {
    #[error("invalid deployment state: {0}")]
    Invalid(String),
    #[error("deployment state conflict: {0}")]
    Conflict(String),
    #[error("deployment state unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub(crate) trait DeploymentStatePort: Send + Sync + 'static {
    async fn create(&self, manifest: DeploymentCrashManifest) -> Result<(), DeploymentStateError>;
    async fn update(
        &self,
        expected_revision: u64,
        manifest: DeploymentCrashManifest,
    ) -> Result<(), DeploymentStateError>;
    async fn remove(
        &self,
        deployment_id: DeploymentId,
        expected_revision: u64,
    ) -> Result<(), DeploymentStateError>;
    async fn unfinished(&self) -> Result<Vec<DeploymentCrashManifest>, DeploymentStateError>;
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryDeploymentStatePort {
    manifests: parking_lot::Mutex<std::collections::HashMap<DeploymentId, DeploymentCrashManifest>>,
}

#[cfg(test)]
#[async_trait]
impl DeploymentStatePort for MemoryDeploymentStatePort {
    async fn create(&self, manifest: DeploymentCrashManifest) -> Result<(), DeploymentStateError> {
        manifest.validate()?;
        let mut manifests = self.manifests.lock();
        if manifests.contains_key(&manifest.deployment_id) {
            return Err(DeploymentStateError::Conflict(
                "deployment manifest already exists".into(),
            ));
        }
        manifests.insert(manifest.deployment_id, manifest);
        Ok(())
    }

    async fn update(
        &self,
        expected_revision: u64,
        manifest: DeploymentCrashManifest,
    ) -> Result<(), DeploymentStateError> {
        manifest.validate()?;
        let mut manifests = self.manifests.lock();
        let current = manifests.get(&manifest.deployment_id).ok_or_else(|| {
            DeploymentStateError::Conflict("deployment manifest is missing".into())
        })?;
        if current.revision != expected_revision || manifest.revision != expected_revision + 1 {
            return Err(DeploymentStateError::Conflict(
                "deployment manifest revision changed".into(),
            ));
        }
        manifests.insert(manifest.deployment_id, manifest);
        Ok(())
    }

    async fn remove(
        &self,
        deployment_id: DeploymentId,
        expected_revision: u64,
    ) -> Result<(), DeploymentStateError> {
        let mut manifests = self.manifests.lock();
        let current = manifests.get(&deployment_id).ok_or_else(|| {
            DeploymentStateError::Conflict("deployment manifest is missing".into())
        })?;
        if current.revision != expected_revision {
            return Err(DeploymentStateError::Conflict(
                "deployment manifest revision changed".into(),
            ));
        }
        manifests.remove(&deployment_id);
        Ok(())
    }

    async fn unfinished(&self) -> Result<Vec<DeploymentCrashManifest>, DeploymentStateError> {
        let mut manifests = self.manifests.lock().values().cloned().collect::<Vec<_>>();
        manifests.sort_by_key(|manifest| manifest.deployment_id.as_uuid());
        Ok(manifests)
    }
}

#[derive(Clone)]
pub(crate) struct DeploymentStateSession {
    port: Arc<dyn DeploymentStatePort>,
    manifest: Arc<tokio::sync::Mutex<DeploymentCrashManifest>>,
}

impl DeploymentStateSession {
    pub(crate) fn new(
        port: Arc<dyn DeploymentStatePort>,
        id: DeploymentId,
        plan: &DeploymentPlan,
    ) -> Self {
        Self {
            port,
            manifest: Arc::new(tokio::sync::Mutex::new(DeploymentCrashManifest::prepared(
                id, plan,
            ))),
        }
    }

    pub(crate) async fn prepare(&self) -> Result<(), DeploymentStateError> {
        self.port.create(self.manifest.lock().await.clone()).await
    }

    pub(crate) async fn applying(
        &self,
        stage: DeploymentStage,
        intended_resources: Vec<DeploymentResource>,
        ledger: &ResourceLedger,
    ) -> Result<(), DeploymentStateError> {
        self.transition(
            DeploymentManifestState::Applying {
                stage,
                intended_resources,
            },
            ledger,
        )
        .await
    }

    pub(crate) async fn committed(
        &self,
        stage: DeploymentStage,
        ledger: &ResourceLedger,
    ) -> Result<(), DeploymentStateError> {
        self.transition(DeploymentManifestState::Committed { stage }, ledger)
            .await
    }

    pub(crate) async fn cleanup_pending(
        &self,
        ledger: &ResourceLedger,
    ) -> Result<(), DeploymentStateError> {
        self.transition(DeploymentManifestState::CleanupPending, ledger)
            .await
    }

    async fn transition(
        &self,
        state: DeploymentManifestState,
        ledger: &ResourceLedger,
    ) -> Result<(), DeploymentStateError> {
        let mut current = self.manifest.lock().await;
        let mut next = current.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| DeploymentStateError::Invalid("manifest revision overflow".into()))?;
        next.state = state;
        next.committed_ledger = ledger.snapshot();
        self.port.update(current.revision, next.clone()).await?;
        *current = next;
        Ok(())
    }

    pub(crate) async fn finish(&self) -> Result<(), DeploymentStateError> {
        let current = self.manifest.lock().await;
        self.port
            .remove(current.deployment_id, current.revision)
            .await
    }

    pub(crate) async fn current_applying_resources(&self) -> Vec<DeploymentResource> {
        let current = self.manifest.lock().await;
        match &current.state {
            DeploymentManifestState::Applying {
                intended_resources, ..
            } => intended_resources.clone(),
            DeploymentManifestState::Prepared
            | DeploymentManifestState::Committed { .. }
            | DeploymentManifestState::CleanupPending => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::MachineProvisioningConfig;
    use super::*;
    use crate::application::provisioning::{DeploymentSource, DeploymentStorage};

    fn request() -> DeploymentRequest {
        DeploymentRequest {
            config: MachineProvisioningConfig {
                name: "test".into(),
                ..Default::default()
            },
            source: DeploymentSource::Copy {
                source_name: "base".into(),
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: Vec::new(),
            allow_unsafe_remote_tar: false,
        }
    }

    #[test]
    fn plan_fingerprint_is_stable_and_changes_with_non_secret_intent() {
        let first = DeploymentPlan::build(request()).unwrap();
        let second = DeploymentPlan::build(request()).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());

        let mut changed = request();
        changed.config.guest_hostname = "different".into();
        let changed = DeploymentPlan::build(changed).unwrap();
        assert_ne!(first.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn plan_fingerprint_is_independent_of_nvidia_destination_insertion_order() {
        use crate::domain::nvidia::{NvidiaFileCategory, NvidiaPassthroughProfile};

        let mut first_profile = NvidiaPassthroughProfile::default();
        first_profile
            .category_destinations
            .insert(NvidiaFileCategory::Lib64, "/opt/nvidia/lib64".into());
        first_profile
            .category_destinations
            .insert(NvidiaFileCategory::Bin, "/opt/nvidia/bin".into());
        let mut second_profile = NvidiaPassthroughProfile::default();
        second_profile
            .category_destinations
            .insert(NvidiaFileCategory::Bin, "/opt/nvidia/bin".into());
        second_profile
            .category_destinations
            .insert(NvidiaFileCategory::Lib64, "/opt/nvidia/lib64".into());

        let mut first_request = request();
        first_request.nvidia_profile = Some(first_profile);
        let mut second_request = request();
        second_request.nvidia_profile = Some(second_profile);

        assert_eq!(
            DeploymentPlan::build(first_request).unwrap().fingerprint(),
            DeploymentPlan::build(second_request).unwrap().fingerprint()
        );
    }

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

    #[test]
    fn outcome_unknown_survives_manifest_validation_and_serialization() {
        let plan = DeploymentPlan::build(request()).unwrap();
        let resource = DeploymentResource::RootfsNetwork(plan.target().clone());
        let mut ledger = ResourceLedger::default();
        ledger.record(resource.clone(), ResourceDisposition::OutcomeUnknown);
        let mut manifest = DeploymentCrashManifest::prepared(DeploymentId::from_u128(8), &plan);
        manifest.state = DeploymentManifestState::CleanupPending;
        manifest.committed_ledger = ledger.snapshot();

        manifest.validate().unwrap();
        let encoded = serde_json::to_vec(&manifest).unwrap();
        let decoded: DeploymentCrashManifest = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, manifest);
        assert_eq!(
            decoded.committed_ledger.entries()[0],
            ResourceLedgerEntry {
                resource,
                disposition: ResourceDisposition::OutcomeUnknown,
            }
        );
    }

    #[test]
    fn manifest_validation_rejects_duplicate_untrusted_resources() {
        let plan = DeploymentPlan::build(request()).unwrap();
        let machine = plan.target().clone();
        let mut manifest = DeploymentCrashManifest::prepared(DeploymentId::new(), &plan);
        manifest.state = DeploymentManifestState::Applying {
            stage: DeploymentStage::HostConfiguration,
            intended_resources: vec![
                DeploymentResource::NspawnConfig(machine.clone()),
                DeploymentResource::NspawnConfig(machine),
            ],
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn manifest_validation_rejects_resources_for_another_target() {
        let plan = DeploymentPlan::build(request()).unwrap();
        let mut manifest = DeploymentCrashManifest::prepared(DeploymentId::new(), &plan);
        manifest.state = DeploymentManifestState::Applying {
            stage: DeploymentStage::HostConfiguration,
            intended_resources: vec![DeploymentResource::NspawnConfig(
                MachineName::new("other").unwrap(),
            )],
        };

        assert!(manifest.validate().is_err());
    }

    #[tokio::test]
    async fn state_session_persists_write_ahead_transitions_and_removes_terminal_state() {
        let plan = DeploymentPlan::build(request()).unwrap();
        let id = DeploymentId::from_u128(9);
        let port = Arc::new(MemoryDeploymentStatePort::default());
        let session = DeploymentStateSession::new(port.clone(), id, &plan);
        let mut ledger = ResourceLedger::default();
        let resource = DeploymentResource::LocalStorage(plan.target().clone());

        session.prepare().await.unwrap();
        session
            .applying(
                DeploymentStage::StoragePreparation,
                vec![resource.clone()],
                &ledger,
            )
            .await
            .unwrap();
        ledger.record(resource, ResourceDisposition::Created);
        session
            .committed(DeploymentStage::StoragePreparation, &ledger)
            .await
            .unwrap();

        let unfinished = port.unfinished().await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].revision, 3);
        assert!(matches!(
            unfinished[0].state,
            DeploymentManifestState::Committed {
                stage: DeploymentStage::StoragePreparation
            }
        ));

        session.finish().await.unwrap();
        assert!(port.unfinished().await.unwrap().is_empty());
    }

    #[test]
    fn manifest_contains_only_the_plan_hash_not_source_credentials() {
        let mut request = request();
        request.source = DeploymentSource::Pull {
            url: "https://user:secret@example.test/rootfs.raw?token=private".into(),
            is_raw: true,
        };
        let plan = DeploymentPlan::build(request).unwrap();
        let manifest = DeploymentCrashManifest::prepared(DeploymentId::from_u128(10), &plan);
        let encoded = serde_json::to_string(&manifest).unwrap();

        assert!(!encoded.contains("user:secret"));
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("example.test"));
    }
}
