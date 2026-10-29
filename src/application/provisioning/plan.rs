//! Validated deployment intent, coordination claims and stable identity.

use crate::application::provisioning::DeploymentError;
use crate::application::{ResourceClaim, ResourceKey};
use crate::domain::machine::MachineName;
use crate::domain::provisioning::DeploymentSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct DeploymentPlan {
    spec: DeploymentSpec,
    target: MachineName,
    fingerprint: PlanFingerprint,
}

impl DeploymentPlan {
    pub(crate) fn build(spec: DeploymentSpec) -> Result<Self, DeploymentError> {
        spec.validate()
            .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        let target = MachineName::new(spec.target_name())
            .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        let normalized = serde_json::to_vec(&spec).map_err(|error| {
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
            spec,
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

    pub(crate) fn into_spec(self) -> DeploymentSpec {
        self.spec
    }

    pub(crate) fn resource_claims(&self) -> Vec<ResourceClaim> {
        let mut claims = vec![ResourceClaim::exclusive(ResourceKey::for_machine(
            &self.target,
        ))];
        if let DeploymentSpec::Clone(deployment) = &self.spec {
            let source = crate::domain::runtime::ImageName::new(&deployment.source_name)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::machine::MachineConfig;

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
    fn plan_fingerprint_is_stable_and_changes_with_non_secret_intent() {
        let first = DeploymentPlan::build(request()).unwrap();
        let second = DeploymentPlan::build(request()).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());

        let mut changed = request();
        let DeploymentSpec::Import(deployment) = &mut changed else {
            unreachable!()
        };
        deployment.machine.guest_hostname = "different".into();
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
        let DeploymentSpec::Import(first) = &mut first_request else {
            unreachable!()
        };
        first.integration.nvidia_profile = Some(first_profile);
        let mut second_request = request();
        let DeploymentSpec::Import(second) = &mut second_request else {
            unreachable!()
        };
        second.integration.nvidia_profile = Some(second_profile);

        assert_eq!(
            DeploymentPlan::build(first_request).unwrap().fingerprint(),
            DeploymentPlan::build(second_request).unwrap().fingerprint()
        );
    }
}
