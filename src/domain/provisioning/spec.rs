use crate::domain::machine::{
    CreateUser, GuestHostname, MachineConfig, MachineName, PrivateUsersMode,
};
use crate::domain::nvidia::{NvidiaCdiSource, NvidiaPassthroughProfile};
use crate::domain::provisioning::bootstrap::BootstrapSpec;
use crate::domain::provisioning::oci::{OciNetworkMode, OciReference};
use crate::domain::provisioning::source::{ArtifactFormat, ArtifactSpec};
use crate::domain::provisioning::storage::DiskImageConfig;
use crate::domain::provisioning::wayland::validate_wayland_intent;
use crate::domain::runtime::ImageName;
use crate::domain::wayland::WaylandGrantIntent;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A deployment carries only the settings its category can apply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "settings", rename_all = "snake_case")]
pub enum DeploymentSpec {
    Bootstrap(Box<BootstrapDeployment>),
    Import(Box<ImportDeployment>),
    Clone(CloneDeployment),
    Oci(OciDeployment),
}

/// Categories also describe the choices available while composing a deployment.
/// Execution matches the specification itself, rather than these UI capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentCategory {
    Bootstrap,
    ArchiveImport,
    DiskImageImport,
    Clone,
    Oci,
}

impl DeploymentCategory {
    pub const fn configures_accounts(self) -> bool {
        matches!(self, Self::Bootstrap)
    }

    pub const fn selects_storage(self) -> bool {
        matches!(self, Self::Bootstrap | Self::ArchiveImport)
    }

    pub const fn configures_machine(self) -> bool {
        matches!(
            self,
            Self::Bootstrap | Self::ArchiveImport | Self::DiskImageImport
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapDeployment {
    pub machine: MachineConfig,
    pub provider: BootstrapSpec,
    pub storage: DeploymentStorage,
    pub users: Vec<CreateUser>,
    pub wayland: Vec<WaylandGrantIntent>,
    pub integration: DeploymentIntegration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportDeployment {
    pub machine: MachineConfig,
    pub image: ImageImport,
    pub integration: DeploymentIntegration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneDeployment {
    pub name: String,
    pub source_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciDeployment {
    pub name: String,
    pub reference: String,
    pub read_only: bool,
    pub network: OciNetworkMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageImport {
    Archive {
        source: ImportSource,
        storage: DeploymentStorage,
        #[serde(default)]
        allow_unsafe_remote_tar: bool,
    },
    DiskImage {
        source: ImportSource,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportSource {
    Local { path: String },
    Remote { url: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeploymentStorage {
    Directory,
    Subvolume,
    DiskImage(DiskImageConfig),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIntegration {
    pub nvidia_profile: Option<NvidiaPassthroughProfile>,
    #[serde(default, skip_serializing_if = "NvidiaCdiSource::is_generate")]
    pub nvidia_cdi_source: NvidiaCdiSource,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DeploymentValidationError(String);

impl DeploymentValidationError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl DeploymentSpec {
    pub fn target_name(&self) -> &str {
        match self {
            Self::Bootstrap(deployment) => &deployment.machine.name,
            Self::Import(deployment) => &deployment.machine.name,
            Self::Clone(deployment) => &deployment.name,
            Self::Oci(deployment) => &deployment.name,
        }
    }

    pub fn category(&self) -> DeploymentCategory {
        match self {
            Self::Bootstrap(_) => DeploymentCategory::Bootstrap,
            Self::Import(deployment) => match deployment.image {
                ImageImport::Archive { .. } => DeploymentCategory::ArchiveImport,
                ImageImport::DiskImage { .. } => DeploymentCategory::DiskImageImport,
            },
            Self::Clone(_) => DeploymentCategory::Clone,
            Self::Oci(_) => DeploymentCategory::Oci,
        }
    }

    pub fn machine_config(&self) -> Option<&MachineConfig> {
        match self {
            Self::Bootstrap(deployment) => Some(&deployment.machine),
            Self::Import(deployment) => Some(&deployment.machine),
            Self::Clone(_) | Self::Oci(_) => None,
        }
    }

    pub fn local_artifact_path(&self) -> Option<&str> {
        match self {
            Self::Import(deployment) => match deployment.image.source() {
                ImportSource::Local { path } => Some(path),
                ImportSource::Remote { .. } => None,
            },
            Self::Bootstrap(_) | Self::Clone(_) | Self::Oci(_) => None,
        }
    }

    pub fn needs_remote_tar_confirmation(&self) -> bool {
        match self {
            Self::Import(deployment) => matches!(
                deployment.image,
                ImageImport::Archive {
                    source: ImportSource::Remote { .. },
                    allow_unsafe_remote_tar: false,
                    ..
                }
            ),
            Self::Bootstrap(_) | Self::Clone(_) | Self::Oci(_) => false,
        }
    }

    pub fn validate(&self) -> Result<(), DeploymentValidationError> {
        let invalid =
            |error: &dyn std::fmt::Display| DeploymentValidationError::invalid(error.to_string());
        let target = MachineName::new(self.target_name()).map_err(|error| invalid(&error))?;
        if let Some(machine) = self.machine_config() {
            GuestHostname::resolve(&machine.guest_hostname, &target)
                .map_err(|error| invalid(&error))?;
            validate_x11(machine)?;
        }
        match self {
            Self::Bootstrap(deployment) => {
                deployment
                    .provider
                    .validate()
                    .map_err(|error| invalid(&error))?;
                deployment.integration.validate()?;
                deployment.validate_accounts()
            }
            Self::Import(deployment) => {
                deployment.integration.validate()?;
                deployment.image.validate()
            }
            Self::Clone(deployment) => {
                ImageName::new(&deployment.source_name).map_err(|error| invalid(&error))?;
                if deployment.source_name == deployment.name {
                    return Err(DeploymentValidationError::invalid(
                        "Clone source and destination must differ",
                    ));
                }
                Ok(())
            }
            Self::Oci(deployment) => OciReference::new(&deployment.reference)
                .map(|_| ())
                .map_err(|error| invalid(&error)),
        }
    }
}

impl BootstrapDeployment {
    fn validate_accounts(&self) -> Result<(), DeploymentValidationError> {
        let mut usernames = HashSet::new();
        let mut requested_uids = HashSet::new();
        for user in &self.users {
            user.validate()
                .map_err(|error| DeploymentValidationError::invalid(error.to_string()))?;
            if !usernames.insert(&user.username) {
                return Err(DeploymentValidationError::invalid(format!(
                    "multiple users request name {:?}",
                    user.username
                )));
            }
            if let Some(uid) = user.uid {
                if !requested_uids.insert(uid) {
                    return Err(DeploymentValidationError::invalid(format!(
                        "multiple users request uid {uid}"
                    )));
                }
            }
        }
        let mut wayland_targets = HashSet::new();
        let mut wayland_sources = HashSet::new();
        for intent in &self.wayland {
            let user = self
                .users
                .iter()
                .find(|user| user.username == intent.target_username())
                .ok_or_else(|| {
                    DeploymentValidationError::invalid(
                        "Wayland target must be one of the users created by this deployment",
                    )
                })?;
            if !wayland_targets.insert(intent.target_username()) {
                return Err(DeploymentValidationError::invalid(
                    "a container user may have only one Wayland access intent",
                ));
            }
            for source in intent.sources() {
                if !wayland_sources.insert(source.canonical_path()) {
                    return Err(DeploymentValidationError::invalid(
                        "a host Wayland socket may be granted only once",
                    ));
                }
            }
            if user.uid != Some(intent.required_uid()) {
                return Err(DeploymentValidationError::invalid(format!(
                    "Wayland target {} must request host session uid {}",
                    user.username,
                    intent.required_uid()
                )));
            }
            validate_wayland_intent(intent, self.machine.private_users)?;
        }
        Ok(())
    }
}

impl DeploymentIntegration {
    fn validate(&self) -> Result<(), DeploymentValidationError> {
        self.nvidia_cdi_source
            .validate()
            .map_err(DeploymentValidationError::invalid)
    }
}

impl ImageImport {
    pub fn source(&self) -> &ImportSource {
        match self {
            Self::Archive { source, .. } | Self::DiskImage { source } => source,
        }
    }

    fn validate(&self) -> Result<(), DeploymentValidationError> {
        match self.source() {
            ImportSource::Local { path } => {
                let format =
                    match self {
                        Self::Archive {
                            allow_unsafe_remote_tar: true,
                            ..
                        } => return Err(DeploymentValidationError::invalid(
                            "Remote archive acknowledgement cannot be used for a local artifact",
                        )),
                        Self::Archive { .. } => ArtifactFormat::Tar,
                        Self::DiskImage { .. } => ArtifactFormat::Raw,
                    };
                ArtifactSpec {
                    path: path.clone(),
                    format,
                }
                .validate()
                .map_err(|error| DeploymentValidationError::invalid(error.to_string()))
            }
            ImportSource::Remote { url } => {
                let parsed = url::Url::parse(url).map_err(|error| {
                    DeploymentValidationError::invalid(format!("Invalid image URL: {error}"))
                })?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err(DeploymentValidationError::invalid(
                        "Image downloads require an HTTP or HTTPS URL with a host",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn validate_x11(machine: &MachineConfig) -> Result<(), DeploymentValidationError> {
    if !machine.x11_binds.is_empty()
        && matches!(
            machine.private_users,
            Some(PrivateUsersMode::Managed | PrivateUsersMode::Identity)
        )
    {
        return Err(DeploymentValidationError::invalid(
            "Host X11 projections are not supported with PrivateUsers=managed or identity",
        ));
    }
    let mut targets = HashSet::new();
    let mut directory = false;
    let mut socket = false;
    for bind in &machine.x11_binds {
        if !targets.insert(bind.target()) {
            return Err(DeploymentValidationError::invalid(
                "an X11 bind target may be selected only once",
            ));
        }
        directory |= bind.is_directory();
        socket |= !bind.is_directory();
    }
    if directory && socket {
        return Err(DeploymentValidationError::invalid(
            "X11 directory and individual socket projections cannot be combined",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
