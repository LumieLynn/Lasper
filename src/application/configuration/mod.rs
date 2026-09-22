//! Consumer-owned configuration inspection contracts. Editing capabilities are
//! added here as their bounded preview/apply operations become available.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::inspection::ResourceInspectionError;
use crate::application::operations::ResourceConflict;
use crate::application::x11::{X11EndpointCatalog, X11EndpointDiscoveryService};
use crate::application::{OperationRegistry, ResourceClaim, ResourceKey};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry};

mod x11;

pub use x11::{X11BindRecommendation, X11BindingChange, X11BindingDeclaration, X11BindingScope};

/// A catalog resource to inspect, not an arbitrary path or a claim that an
/// image and a running machine with the same name share a launch source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "name",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConfigurationTarget {
    Machine(MachineName),
    Image(ImageName),
}

impl ConfigurationTarget {
    pub fn for_machine(machine: &MachineEntry) -> Result<Self, ResourceInspectionError> {
        if !machine.access().is_nspawn() {
            return Err(ResourceInspectionError::unsupported(
                "Configure requires a systemd-nspawn machine",
            ));
        }
        machine
            .validated_name()
            .map(Self::Machine)
            .map_err(ResourceInspectionError::backend)
    }

    pub fn for_image(image: &ImageEntry) -> Result<Self, ResourceInspectionError> {
        ImageName::new(&image.name)
            .map(Self::Image)
            .map_err(ResourceInspectionError::backend)
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Machine(name) => name.as_str(),
            Self::Image(name) => name.as_str(),
        }
    }
}

/// The extent of discovery is explicit. Neither existing reader resolves
/// arbitrary nspawn invocations, custom units, or command-line overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationDiscovery {
    MachineNameCandidates,
    NamedImageCandidates,
}

impl ConfigurationDiscovery {
    pub fn description(self) -> &'static str {
        match self {
            Self::MachineNameCandidates => {
                "Machine-name search: administrator, then runtime settings. The root source and launch overrides are not resolved."
            }
            Self::NamedImageCandidates => {
                "Image-name search: administrator, runtime, then readable image-adjacent settings. A launch target is not inferred."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationOrigin {
    Administrator,
    Runtime,
    ImageAdjacent,
}

impl ConfigurationOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Administrator => "Administrator configuration",
            Self::Runtime => "Runtime configuration",
            Self::ImageAdjacent => "Image-adjacent configuration; trust policy not verified",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ConfigurationDocument {
    pub path: PathBuf,
    pub origin: ConfigurationOrigin,
    pub content: String,
    /// A fingerprint of the displayed bytes only. This is not an apply token:
    /// discovery, metadata and the write target also need revalidation.
    pub content_sha256: String,
}

/// Observations follow search order. A failed earlier candidate is not treated
/// as absent, and later candidates do not silently take its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum ConfigurationCandidateState {
    Absent,
    Selected,
    NotConsulted,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationCandidate {
    pub path: PathBuf,
    pub origin: ConfigurationOrigin,
    pub state: ConfigurationCandidateState,
}

/// Independent preconditions for the declared file search. This does not
/// certify runtime-effective settings or grant permission to promote an
/// image-adjacent document into a trusted administrator file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationRevision {
    pub discovery: String,
    pub read_source: Option<String>,
    pub write_target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationWriteTarget {
    /// Derived by the executor from a valid effective machine name.
    pub path: PathBuf,
    pub exists: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ConfigurationSnapshot {
    pub target: ConfigurationTarget,
    pub discovery: ConfigurationDiscovery,
    pub document: Option<ConfigurationDocument>,
    pub candidates: Vec<ConfigurationCandidate>,
    /// Missing when the search could not be completed consistently.
    pub revision: Option<ConfigurationRevision>,
    pub write_target: Option<ConfigurationWriteTarget>,
    pub x11_bindings: Vec<X11BindingDeclaration>,
    pub x11_bind_recommendation: X11BindRecommendation,
    pub host_x11: X11EndpointCatalog,
    pub other_bind_count: usize,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigurationWriteError {
    #[error("The draft target does not match the inspected resource")]
    TargetMismatch,
    #[error("Configuration discovery was incomplete; resolve the source error before editing")]
    DiscoveryIncomplete,
    #[error("The selected configuration source or file revision changed; refresh before saving")]
    RevisionChanged,
    #[error("No existing administrator configuration contains this declaration")]
    NoDocument,
    #[error("{0} is inspect-only; creating an administrator file would change source precedence")]
    NonAdministratorSource(String),
    #[error("This resource cannot be mapped to a safe administrator write target")]
    NoWriteTarget,
    #[error("The selected source is not the existing administrator write target")]
    WriteTargetMismatch,
}

/// Common, validated write facts shared by every configuration page.
///
/// Page adapters may use the document bytes to calculate their own bounded
/// mutations, but they must not reimplement source precedence or revision
/// checks.
pub(crate) struct ConfigurationWriteContext<'a> {
    pub(crate) document: &'a ConfigurationDocument,
}

impl ConfigurationSnapshot {
    pub(crate) fn write_context<'a>(
        &'a self,
        target: &ConfigurationTarget,
        base_revision: &ConfigurationRevision,
    ) -> Result<ConfigurationWriteContext<'a>, ConfigurationWriteError> {
        if &self.target != target {
            return Err(ConfigurationWriteError::TargetMismatch);
        }
        let Some(revision) = &self.revision else {
            return Err(ConfigurationWriteError::DiscoveryIncomplete);
        };
        if revision != base_revision {
            return Err(ConfigurationWriteError::RevisionChanged);
        }
        let Some(document) = &self.document else {
            return Err(ConfigurationWriteError::NoDocument);
        };
        if document.origin != ConfigurationOrigin::Administrator {
            return Err(ConfigurationWriteError::NonAdministratorSource(
                document.origin.label().into(),
            ));
        }
        let Some(write_target) = &self.write_target else {
            return Err(ConfigurationWriteError::NoWriteTarget);
        };
        if !write_target.exists || write_target.path != document.path {
            return Err(ConfigurationWriteError::WriteTargetMismatch);
        }
        Ok(ConfigurationWriteContext { document })
    }
}

/// A finite configuration draft. The caller identifies declarations from the
/// inspected revision; it never supplies a host configuration path or a whole
/// replacement document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationEdit {
    pub target: ConfigurationTarget,
    pub base_revision: ConfigurationRevision,
    pub x11_changes: Vec<X11BindingChange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationActivation {
    NextMachineStart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConfigurationPreview {
    Ready {
        path: PathBuf,
        diff: String,
        activation: ConfigurationActivation,
    },
    Unchanged {
        path: PathBuf,
    },
    Blocked {
        reason: String,
    },
    Conflict {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConfigurationApplyReport {
    Applied {
        path: PathBuf,
        activation: ConfigurationActivation,
    },
    Unchanged {
        path: PathBuf,
    },
    Blocked {
        reason: String,
    },
    Conflict {
        reason: String,
    },
    Busy {
        reason: String,
    },
}

impl fmt::Debug for ConfigurationSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigurationSnapshot")
            .field("target", &self.target)
            .field("discovery", &self.discovery)
            .field("document_present", &self.document.is_some())
            .field("x11_binding_count", &self.x11_bindings.len())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
pub(crate) trait ConfigurationPort: Send + Sync {
    async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ResourceInspectionError>;

    async fn preview(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationPreview, ResourceInspectionError>;

    async fn apply(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationApplyReport, ResourceInspectionError>;
}

pub struct ConfigurationService {
    port: Arc<dyn ConfigurationPort>,
    x11_endpoints: Arc<X11EndpointDiscoveryService>,
    operations: Arc<OperationRegistry>,
}

impl ConfigurationService {
    pub(crate) fn new(
        port: Arc<dyn ConfigurationPort>,
        x11_endpoints: Arc<X11EndpointDiscoveryService>,
        operations: Arc<OperationRegistry>,
    ) -> Self {
        Self {
            port,
            x11_endpoints,
            operations,
        }
    }

    pub async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ResourceInspectionError> {
        let mut snapshot = self.port.inspect(target).await?;
        let sources = snapshot
            .x11_bindings
            .iter()
            .map(|binding| binding.source.clone())
            .collect::<Vec<_>>();
        snapshot.host_x11 = self.x11_endpoints.discover(&sources).await;
        Ok(snapshot)
    }

    pub async fn preview(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationPreview, ResourceInspectionError> {
        self.port.preview(edit).await
    }

    pub async fn apply(
        &self,
        edit: &ConfigurationEdit,
    ) -> Result<ConfigurationApplyReport, ResourceInspectionError> {
        let key = match &edit.target {
            ConfigurationTarget::Machine(machine) => ResourceKey::for_machine(machine),
            ConfigurationTarget::Image(image) => ResourceKey::for_image(image),
        };
        let _reservation = match self.operations.reserve([ResourceClaim::exclusive(key)]) {
            Ok(reservation) => reservation,
            Err(ResourceConflict { .. }) => {
                return Ok(ConfigurationApplyReport::Busy {
                    reason: "Another operation is using this machine configuration".into(),
                });
            }
        };
        self.port.apply(edit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::x11::{X11EndpointCatalog, X11EndpointDiscoveryPort};
    use tokio::sync::Notify;

    struct EmptyX11Endpoints;

    #[async_trait::async_trait]
    impl X11EndpointDiscoveryPort for EmptyX11Endpoints {
        async fn discover(&self, _configured_sources: &[PathBuf]) -> X11EndpointCatalog {
            X11EndpointCatalog::default()
        }
    }

    struct BlockingPort {
        entered: Notify,
        release: Notify,
    }

    #[async_trait::async_trait]
    impl ConfigurationPort for BlockingPort {
        async fn inspect(
            &self,
            _target: &ConfigurationTarget,
        ) -> Result<ConfigurationSnapshot, ResourceInspectionError> {
            unreachable!("this test only exercises apply coordination")
        }

        async fn preview(
            &self,
            _edit: &ConfigurationEdit,
        ) -> Result<ConfigurationPreview, ResourceInspectionError> {
            unreachable!("this test only exercises apply coordination")
        }

        async fn apply(
            &self,
            _edit: &ConfigurationEdit,
        ) -> Result<ConfigurationApplyReport, ResourceInspectionError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ConfigurationApplyReport::Applied {
                path: "/etc/systemd/nspawn/arch.nspawn".into(),
                activation: ConfigurationActivation::NextMachineStart,
            })
        }
    }

    fn edit() -> ConfigurationEdit {
        ConfigurationEdit {
            target: ConfigurationTarget::Machine(MachineName::new("arch").unwrap()),
            base_revision: ConfigurationRevision {
                discovery: "discovery".into(),
                read_source: Some("source".into()),
                write_target: Some("target".into()),
            },
            x11_changes: vec![X11BindingChange::Remove { line: 2 }],
        }
    }

    #[tokio::test]
    async fn apply_reserves_the_machine_configuration_until_the_port_finishes() {
        let port = Arc::new(BlockingPort {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let service = Arc::new(ConfigurationService::new(
            port.clone(),
            Arc::new(X11EndpointDiscoveryService::new(Arc::new(
                EmptyX11Endpoints,
            ))),
            OperationRegistry::new(),
        ));
        let request = edit();
        let first = {
            let service = service.clone();
            let request = request.clone();
            tokio::spawn(async move { service.apply(&request).await.unwrap() })
        };
        port.entered.notified().await;

        assert!(matches!(
            service.apply(&request).await.unwrap(),
            ConfigurationApplyReport::Busy { .. }
        ));
        port.release.notify_one();
        assert!(matches!(
            first.await.unwrap(),
            ConfigurationApplyReport::Applied { .. }
        ));
    }
}
