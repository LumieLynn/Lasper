//! Host X11 endpoint discovery contracts.
//!
//! This module only describes and discovers host-side endpoints. ACL state,
//! guest projection evidence, and lifecycle authorization belong to the
//! sibling access/service modules.

use std::path::PathBuf;
use std::sync::Arc;

use crate::domain::x11::HostX11Socket;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11EndpointCatalog {
    pub sockets: Vec<HostX11Socket>,
    /// Observations of configured sources, including sources that could not
    /// become authenticated endpoints. Absence from the socket list is not
    /// absence from the filesystem.
    pub sources: Vec<X11SourceObservation>,
    pub preferred_display: Option<u16>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct X11SourceObservation {
    pub source: PathBuf,
    pub state: X11SourceState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum X11SourceState {
    Observed,
    Missing,
    Invalid(String),
    Unverified(String),
}

#[async_trait::async_trait]
pub(crate) trait X11EndpointDiscoveryPort: Send + Sync {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog;
}

pub(crate) struct X11EndpointDiscoveryService {
    port: Arc<dyn X11EndpointDiscoveryPort>,
}

impl X11EndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn X11EndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        self.port.discover(configured_sources).await
    }
}
