//! Read-only recovery evidence for interrupted deployments.

use super::{
    DeploymentCrashManifest, DeploymentError, DeploymentManifestState, DeploymentResource,
    ResourceDisposition,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentRecoveryResource {
    pub(crate) resource: DeploymentResource,
    pub(crate) recorded_disposition: Option<ResourceDisposition>,
    pub(crate) applying_when_interrupted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "evidence", content = "detail", rename_all = "snake_case")]
pub(crate) enum DeploymentRecoveryEvidence {
    Present,
    Absent,
    ProvenOwned,
    Ambiguous,
    NotProbeable,
    ProbeFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentRecoveryObservation {
    pub(crate) subject: DeploymentRecoveryResource,
    pub(crate) evidence: DeploymentRecoveryEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeploymentRecoveryReport {
    pub(crate) manifest: DeploymentCrashManifest,
    pub(crate) observations: Vec<DeploymentRecoveryObservation>,
    pub(crate) probe_error: Option<String>,
}

#[async_trait]
pub(crate) trait DeploymentRecoveryProbe: Send + Sync + 'static {
    async fn probe(
        &self,
        manifest: &DeploymentCrashManifest,
    ) -> Result<Vec<DeploymentRecoveryObservation>, DeploymentError>;
}

impl DeploymentCrashManifest {
    pub(crate) fn recovery_resources(&self) -> Vec<DeploymentRecoveryResource> {
        let mut resources = self
            .committed_ledger
            .entries()
            .iter()
            .map(|entry| DeploymentRecoveryResource {
                resource: entry.resource.clone(),
                recorded_disposition: Some(entry.disposition),
                applying_when_interrupted: false,
            })
            .collect::<Vec<_>>();

        if let DeploymentManifestState::Applying {
            intended_resources, ..
        } = &self.state
        {
            for intended in intended_resources {
                if let Some(existing) = resources
                    .iter_mut()
                    .find(|subject| subject.resource == *intended)
                {
                    existing.applying_when_interrupted = true;
                } else {
                    resources.push(DeploymentRecoveryResource {
                        resource: intended.clone(),
                        recorded_disposition: None,
                        applying_when_interrupted: true,
                    });
                }
            }
        }

        resources
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryDeploymentRecoveryProbe;

#[cfg(test)]
#[async_trait]
impl DeploymentRecoveryProbe for MemoryDeploymentRecoveryProbe {
    async fn probe(
        &self,
        manifest: &DeploymentCrashManifest,
    ) -> Result<Vec<DeploymentRecoveryObservation>, DeploymentError> {
        Ok(manifest
            .recovery_resources()
            .into_iter()
            .map(|subject| DeploymentRecoveryObservation {
                subject,
                evidence: DeploymentRecoveryEvidence::NotProbeable,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{
        DeploymentId, DeploymentPlan, DeploymentRequest, DeploymentSource, DeploymentStage,
        DeploymentStorage, ResourceLedger,
    };
    use crate::nspawn::models::{ContainerConfig, MachineName};

    fn manifest() -> DeploymentCrashManifest {
        let plan = DeploymentPlan::build(DeploymentRequest {
            config: ContainerConfig {
                name: "target".into(),
                ..Default::default()
            },
            source: DeploymentSource::Copy {
                source_name: "base".into(),
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: Vec::new(),
            allow_unsafe_remote_tar: false,
        })
        .unwrap();
        DeploymentCrashManifest::prepared(DeploymentId::from_u128(1), &plan)
    }

    #[test]
    fn recovery_resources_merge_committed_and_interrupted_effects() {
        let target = MachineName::new("target").unwrap();
        let mut ledger = ResourceLedger::default();
        ledger.record(
            DeploymentResource::LocalStorage(target.clone()),
            ResourceDisposition::Created,
        );
        ledger.record(
            DeploymentResource::NspawnConfig(target.clone()),
            ResourceDisposition::PreExisting,
        );
        let mut manifest = manifest();
        manifest.committed_ledger = ledger.snapshot();
        manifest.state = DeploymentManifestState::Applying {
            stage: DeploymentStage::HostConfiguration,
            intended_resources: vec![
                DeploymentResource::NspawnConfig(target.clone()),
                DeploymentResource::SystemdOverride(target),
            ],
        };

        let resources = manifest.recovery_resources();
        assert_eq!(resources.len(), 3);
        assert_eq!(
            resources[0].recorded_disposition,
            Some(ResourceDisposition::Created)
        );
        assert!(!resources[0].applying_when_interrupted);
        assert_eq!(
            resources[1].recorded_disposition,
            Some(ResourceDisposition::PreExisting)
        );
        assert!(resources[1].applying_when_interrupted);
        assert_eq!(resources[2].recorded_disposition, None);
        assert!(resources[2].applying_when_interrupted);
    }
}
