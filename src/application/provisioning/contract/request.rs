use crate::domain::bootstrap::BootstrapSpec;
use crate::domain::machine::{GuestHostname, MachineName};
use crate::domain::nvidia::{NvidiaCdiSource, NvidiaPassthroughProfile};
use crate::domain::provisioning::OciNetworkMode;
use crate::domain::source::ArtifactSpec;
use crate::domain::storage::DiskImageConfig;
use crate::domain::wayland::WaylandGrantIntent;
use serde::{Deserialize, Serialize};

use super::config::MachineProvisioningConfig;

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
    pub config: MachineProvisioningConfig,
    pub source: DeploymentSource,
    pub storage: DeploymentStorage,
    pub nvidia_profile: Option<NvidiaPassthroughProfile>,
    #[serde(default, skip_serializing_if = "NvidiaCdiSource::is_generate")]
    pub nvidia_cdi_source: NvidiaCdiSource,
    pub wayland: Vec<WaylandGrantIntent>,
    pub allow_unsafe_remote_tar: bool,
}

impl DeploymentRequest {
    pub(crate) fn validate(&self) -> Result<(), super::job::DeploymentError> {
        self.nvidia_cdi_source
            .validate()
            .map_err(super::job::DeploymentError::rejected)?;
        let machine = MachineName::new(self.config.name.clone())
            .map_err(|error| super::job::DeploymentError::rejected(error.to_string()))?;
        GuestHostname::resolve(&self.config.guest_hostname, &machine)
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
        if !self.config.x11_binds.is_empty()
            && matches!(
                self.config.private_users,
                Some(
                    crate::domain::provisioning::PrivateUsersMode::Managed
                        | crate::domain::provisioning::PrivateUsersMode::Identity
                )
            )
        {
            return Err(super::job::DeploymentError::rejected(
                "Host X11 socket binds are not supported with PrivateUsers=managed or identity",
            ));
        }
        let mut x11_targets = std::collections::HashSet::new();
        for bind in &self.config.x11_binds {
            if !self.source.supports_rootfs_configuration() {
                return Err(super::job::DeploymentError::rejected(
                    "Host X11 socket binds require a deployment source that supports rootfs configuration",
                ));
            }
            if !x11_targets.insert(bind.target().to_path_buf()) {
                return Err(super::job::DeploymentError::rejected(
                    "an X11 bind target may be selected only once",
                ));
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
