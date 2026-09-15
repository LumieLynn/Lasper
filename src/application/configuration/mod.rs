//! Consumer-owned configuration inspection contracts. Editing capabilities are
//! added here as their bounded preview/apply operations become available.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::inspection::ResourceInspectionError;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum X11BindingScope {
    Directory,
    Socket { display: u16, alternate: bool },
}

/// A declaration recognizable from the standard host X11 path. The display
/// number is a filename hint, not evidence of a live server or authorization.
#[derive(Serialize, Deserialize)]
pub struct X11BindingDeclaration {
    pub line: usize,
    pub source: PathBuf,
    pub guest_target: PathBuf,
    pub readonly: bool,
    pub options: Vec<String>,
    pub scope: X11BindingScope,
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
    pub other_bind_count: usize,
    pub diagnostics: Vec<String>,
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
}

pub struct ConfigurationService {
    port: Arc<dyn ConfigurationPort>,
}

impl ConfigurationService {
    pub(crate) fn new(port: Arc<dyn ConfigurationPort>) -> Self {
        Self { port }
    }

    pub async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ResourceInspectionError> {
        self.port.inspect(target).await
    }
}
