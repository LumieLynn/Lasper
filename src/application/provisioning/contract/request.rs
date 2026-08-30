use crate::domain::nvidia::NvidiaPassthroughProfile;
use crate::domain::provisioning::OciNetworkMode;
use crate::domain::source::ArtifactSpec;
use crate::domain::storage::DiskImageConfig;
use crate::domain::wayland::WaylandGrantIntent;
use crate::nspawn::models::{BootstrapSpec, ContainerConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeploymentSource {
    Copy {
        source_name: String,
    },
    Oci {
        reference: String,
        read_only: bool,
        network: OciNetworkMode,
    },
    Bootstrap(BootstrapSpec),
    Pull {
        url: String,
        is_raw: bool,
    },
    Artifact(ArtifactSpec),
}

impl DeploymentSource {
    pub fn is_unacknowledged_remote_tar(&self, acknowledged: bool) -> bool {
        matches!(self, Self::Pull { is_raw: false, .. }) && !acknowledged
    }

    pub fn supports_rootfs_configuration(&self) -> bool {
        !matches!(self, Self::Copy { .. } | Self::Oci { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeploymentStorage {
    Directory,
    Subvolume,
    DiskImage(DiskImageConfig),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentRequest {
    pub config: ContainerConfig,
    pub source: DeploymentSource,
    pub storage: DeploymentStorage,
    pub nvidia_profile: Option<NvidiaPassthroughProfile>,
    pub wayland: Vec<WaylandGrantIntent>,
    pub allow_unsafe_remote_tar: bool,
}

impl DeploymentRequest {
    pub(crate) fn validate(&self) -> Result<(), super::job::DeploymentError> {
        crate::nspawn::models::NspawnConfigSpec::try_from(&self.config)
            .map_err(|error| super::job::DeploymentError::rejected(error.to_string()))?;
        if let DeploymentSource::Copy { source_name } = &self.source {
            crate::domain::runtime::ImageName::new(source_name).map_err(|error| {
                super::job::DeploymentError::rejected(format!("Invalid clone source: {error}"))
            })?;
        }
        for user in &self.config.users {
            user.validate()
                .map_err(|error| super::job::DeploymentError::rejected(error.to_string()))?;
        }
        let mut requested_uids = std::collections::HashSet::new();
        for uid in self.config.users.iter().filter_map(|user| user.uid) {
            if !requested_uids.insert(uid) {
                return Err(super::job::DeploymentError::rejected(format!(
                    "multiple users request uid {uid}"
                )));
            }
        }
        let mut wayland_targets = std::collections::HashSet::new();
        let mut wayland_sources = std::collections::HashSet::new();
        for intent in &self.wayland {
            if !self.source.supports_rootfs_configuration() {
                return Err(super::job::DeploymentError::rejected(
                    "Wayland grants require a deployment source that supports rootfs user configuration",
                ));
            }
            let Some(user) = self
                .config
                .users
                .iter()
                .find(|user| user.username == intent.target_username())
            else {
                return Err(super::job::DeploymentError::rejected(
                    "Wayland target must be one of the users created by this deployment",
                ));
            };
            if !wayland_targets.insert(intent.target_username()) {
                return Err(super::job::DeploymentError::rejected(
                    "a container user may have only one Wayland access intent",
                ));
            }
            for source in intent.sources() {
                if !wayland_sources.insert(source.canonical_path()) {
                    return Err(super::job::DeploymentError::rejected(
                        "a host Wayland socket may be granted only once",
                    ));
                }
            }
            user.validate()
                .map_err(|error| super::job::DeploymentError::rejected(error.to_string()))?;
            if user.uid != Some(intent.required_uid()) {
                return Err(super::job::DeploymentError::rejected(format!(
                    "Wayland target {} must request host session uid {}",
                    user.username,
                    intent.required_uid(),
                )));
            }
            crate::application::provisioning::wayland::validate_wayland_intent(
                intent,
                self.config.private_users,
            )?;
        }
        Ok(())
    }
}
