//! Durable deployment stages, crash manifests and revision-checked persistence.

use crate::application::provisioning::ownership::{
    DeploymentResource, ResourceLedger, ResourceLedgerSnapshot,
};
use crate::application::provisioning::plan::{DeploymentPlan, PlanFingerprint};
use crate::application::provisioning::DeploymentId;
use crate::application::{ResourceClaim, ResourceKey};
use crate::domain::machine::MachineName;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u32 = 1;

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
        self.committed_ledger
            .validate(&self.target)
            .map_err(|reason| DeploymentStateError::Invalid(reason.into()))?;
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
    use super::*;
    use crate::application::provisioning::ownership::{ResourceDisposition, ResourceLedgerEntry};
    use crate::domain::machine::MachineConfig;
    use crate::domain::provisioning::DeploymentSpec;

    fn request() -> DeploymentSpec {
        DeploymentSpec::Import(Box::new(crate::domain::provisioning::ImportDeployment {
            machine: MachineConfig {
                name: "test".into(),
                ..Default::default()
            },
            image: crate::domain::provisioning::ImageImport::DiskImage {
                source: crate::domain::provisioning::ImportSource::Remote {
                    url: "https://example.test/rootfs.raw".into(),
                },
            },
            integration: Default::default(),
        }))
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
        let DeploymentSpec::Import(deployment) = &mut request else {
            unreachable!()
        };
        deployment.image = crate::domain::provisioning::ImageImport::DiskImage {
            source: crate::domain::provisioning::ImportSource::Remote {
                url: "https://user:secret@example.test/rootfs.raw?token=private".into(),
            },
        };
        let plan = DeploymentPlan::build(request).unwrap();
        let manifest = DeploymentCrashManifest::prepared(DeploymentId::from_u128(10), &plan);
        let encoded = serde_json::to_string(&manifest).unwrap();

        assert!(!encoded.contains("user:secret"));
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("example.test"));
    }
}
