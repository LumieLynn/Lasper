//! Private wire contract for daemon-owned provisioning jobs.

use crate::application::provisioning::{
    DeploymentEvent, DeploymentId, DeploymentRecoveryObservation, DeploymentRequest,
    DeploymentRequestId, DeploymentSecretsWire, DeploymentStatus,
};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_DEPLOYMENT_STREAM_FRAME_BYTES: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitDeploymentParams {
    pub(crate) request_id: DeploymentRequestId,
    pub(crate) deployment_id: DeploymentId,
    pub(crate) request: DeploymentRequest,
    pub(crate) secrets: DeploymentSecretsWire,
}

impl std::fmt::Debug for SubmitDeploymentParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitDeploymentParams")
            .field("request_id", &self.request_id)
            .field("deployment_id", &self.deployment_id)
            .field("request", &self.request)
            .field("secrets", &self.secrets)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "frame", content = "payload", rename_all = "snake_case")]
pub(crate) enum DeploymentStreamFrame {
    Event(DeploymentEvent),
    Snapshot(DeploymentJobSnapshot),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentJobRequest {
    pub(crate) deployment_id: DeploymentId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentJobSnapshot {
    pub(crate) deployment_id: DeploymentId,
    pub(crate) revision: u64,
    pub(crate) status: DeploymentStatus,
    pub(crate) cancellation_requested: bool,
    pub(crate) claim: DeploymentClaimState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeploymentClaimState {
    Held,
    Released,
    ReconciliationRequired,
    Reconciled,
    ReleasedUnresolved,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentSubmissionRequest {
    pub(crate) request_id: DeploymentRequestId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "submission", content = "payload", rename_all = "snake_case")]
pub(crate) enum DeploymentSubmissionStatus {
    Pending,
    Accepted { deployment_id: DeploymentId },
    Rejected { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentSubmissionSnapshot {
    pub(crate) request_id: DeploymentRequestId,
    pub(crate) status: DeploymentSubmissionStatus,
    pub(crate) acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseUnresolvedDeploymentRequest {
    pub(crate) deployment_id: DeploymentId,
    pub(crate) confirmed: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeDeploymentRecoveryRequest {
    pub(crate) deployment_id: DeploymentId,
    pub(crate) expected_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeDeploymentRecoveryResult {
    pub(crate) deployment_id: DeploymentId,
    pub(crate) manifest_revision: u64,
    pub(crate) observations: Vec<DeploymentRecoveryObservation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::MachineProvisioningConfig;
    use crate::application::provisioning::{
        DeploymentSecrets, DeploymentSource, DeploymentStorage,
    };

    #[test]
    fn submission_debug_redacts_wire_secrets_and_json_keeps_them_in_the_capsule() {
        let params = SubmitDeploymentParams {
            request_id: DeploymentRequestId::from_u128(1),
            deployment_id: DeploymentId::from_u128(2),
            request: DeploymentRequest {
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
            },
            secrets: DeploymentSecrets::new("root-secret".into(), Vec::new()).into_wire(),
        };

        let debug = format!("{params:?}");
        assert!(!debug.contains("root-secret"));
        assert!(debug.contains("[REDACTED]"));

        let wire = serde_json::to_value(params).unwrap();
        assert_eq!(wire["secrets"]["root_password"], "root-secret");
        assert!(wire["request"].get("root_password").is_none());
        assert!(wire["request"]["config"].get("password").is_none());
    }
}
