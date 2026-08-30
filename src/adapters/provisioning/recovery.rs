//! Route-specific, read-only host probes for interrupted deployments.

use crate::adapters::config::store::{probe_exact_nspawn_config, NspawnConfigPresence};
use crate::adapters::config::SystemdUnitStore;
use crate::adapters::platform::nvidia::NvidiaStateStore;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::ArtifactOwnership;
use crate::application::provisioning::{
    DeploymentCrashManifest, DeploymentError, DeploymentRecoveryEvidence,
    DeploymentRecoveryObservation, DeploymentRecoveryProbe, DeploymentResource,
};
use crate::application::RuntimeCatalog;
use crate::domain::runtime::ImageEntry;
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) struct DirectDeploymentRecoveryProbe {
    runtime: Arc<RuntimeCatalog>,
    state_root: TrustedStateRoot,
}

impl DirectDeploymentRecoveryProbe {
    pub(crate) fn new(runtime: Arc<RuntimeCatalog>, state_root: TrustedStateRoot) -> Self {
        Self {
            runtime,
            state_root,
        }
    }
}

#[async_trait]
impl DeploymentRecoveryProbe for DirectDeploymentRecoveryProbe {
    async fn probe(
        &self,
        manifest: &DeploymentCrashManifest,
    ) -> Result<Vec<DeploymentRecoveryObservation>, DeploymentError> {
        let images = if requires_runtime_image_probe(manifest) {
            self.runtime
                .snapshot()
                .await
                .map(|snapshot| snapshot.value.images)
                .map_err(|error| error.to_string())
        } else {
            Ok(Vec::new())
        };
        Ok(probe_manifest_locally(manifest, images, self.state_root.clone()).await)
    }
}

pub(crate) fn requires_runtime_image_probe(manifest: &DeploymentCrashManifest) -> bool {
    manifest.recovery_resources().iter().any(|subject| {
        matches!(
            subject.resource,
            DeploymentResource::LocalStorage(_) | DeploymentResource::ExternalImage(_)
        )
    })
}

pub(crate) struct ElevatedDeploymentRecoveryProbe {
    daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
}

impl ElevatedDeploymentRecoveryProbe {
    pub(crate) fn new(daemon: Arc<crate::adapters::elevated::ElevatedDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl DeploymentRecoveryProbe for ElevatedDeploymentRecoveryProbe {
    async fn probe(
        &self,
        manifest: &DeploymentCrashManifest,
    ) -> Result<Vec<DeploymentRecoveryObservation>, DeploymentError> {
        self.daemon
            .probe_deployment_recovery(manifest.deployment_id, manifest.revision)
            .await
            .map_err(|error| {
                DeploymentError::reconciliation_required(format!(
                    "elevated recovery probe failed: {error}"
                ))
            })
    }
}

pub(crate) async fn probe_manifest_locally(
    manifest: &DeploymentCrashManifest,
    images: Result<Vec<ImageEntry>, String>,
    state_root: TrustedStateRoot,
) -> Vec<DeploymentRecoveryObservation> {
    let systemd_units = SystemdUnitStore::direct();
    let nvidia_state = NvidiaStateStore::direct(state_root);
    let mut observations = Vec::new();

    for subject in manifest.recovery_resources() {
        let target = manifest.target.as_str();
        let evidence = match &subject.resource {
            DeploymentResource::LocalStorage(_) | DeploymentResource::ExternalImage(_) => {
                match &images {
                    Ok(images) if images.iter().any(|image| image.name == target) => {
                        DeploymentRecoveryEvidence::Present
                    }
                    Ok(_) => DeploymentRecoveryEvidence::Absent,
                    Err(error) => DeploymentRecoveryEvidence::ProbeFailed(error.clone()),
                }
            }
            DeploymentResource::NspawnConfig(_) => {
                match probe_exact_nspawn_config(&manifest.target).await {
                    Ok(NspawnConfigPresence::Regular) => DeploymentRecoveryEvidence::Present,
                    Ok(NspawnConfigPresence::Absent) => DeploymentRecoveryEvidence::Absent,
                    Ok(NspawnConfigPresence::Unsafe) => DeploymentRecoveryEvidence::Ambiguous,
                    Err(error) => DeploymentRecoveryEvidence::ProbeFailed(error.to_string()),
                }
            }
            DeploymentResource::SystemdOverride(_) => {
                match systemd_units.probe_owned_overrides(target).await {
                    Ok(ownership) => aggregate_ownership(&ownership),
                    Err(error) => DeploymentRecoveryEvidence::ProbeFailed(error.to_string()),
                }
            }
            DeploymentResource::NvidiaState(_) => match nvidia_state.probe_owned(target).await {
                Ok(ownership) => aggregate_ownership(&[ownership]),
                Err(error) => DeploymentRecoveryEvidence::ProbeFailed(error.to_string()),
            },
            DeploymentResource::StorageMount(_)
            | DeploymentResource::RawConfigurationMount(_)
            | DeploymentResource::RootfsHostname(_)
            | DeploymentResource::RootfsAccounts(_)
            | DeploymentResource::RootfsNvidia(_)
            | DeploymentResource::RootfsNetwork(_) => DeploymentRecoveryEvidence::NotProbeable,
        };
        observations.push(DeploymentRecoveryObservation { subject, evidence });
    }

    observations
}

fn aggregate_ownership(ownership: &[ArtifactOwnership]) -> DeploymentRecoveryEvidence {
    if ownership.contains(&ArtifactOwnership::ProvenOwned) {
        DeploymentRecoveryEvidence::ProvenOwned
    } else if ownership.contains(&ArtifactOwnership::AmbiguousLegacy) {
        DeploymentRecoveryEvidence::Ambiguous
    } else {
        DeploymentRecoveryEvidence::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_evidence_never_promotes_ambiguous_artifacts() {
        assert_eq!(
            aggregate_ownership(&[
                ArtifactOwnership::NotPresent,
                ArtifactOwnership::AmbiguousLegacy,
            ]),
            DeploymentRecoveryEvidence::Ambiguous
        );
        assert_eq!(
            aggregate_ownership(&[ArtifactOwnership::NotPresent]),
            DeploymentRecoveryEvidence::Absent
        );
    }
}
