//! Wayland-specific persistent configuration contracts.
//!
//! Runtime projection validation is intentionally owned by the session
//! service rather than in this configuration subdomain.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};

pub fn recommended_wayland_target(uid: u32, display: &WaylandDisplay) -> PathBuf {
    crate::domain::wayland::container_socket_path(uid, display)
}

pub type WaylandBindRecommendation = super::DisplayBindRecommendation;

#[derive(Serialize, Deserialize)]
pub struct WaylandBindingDeclaration {
    pub line: usize,
    pub display: WaylandDisplay,
    pub source: PathBuf,
    pub guest_target: PathBuf,
    pub readonly: bool,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaylandBindingChange {
    Add {
        source: PathBuf,
        guest_target: PathBuf,
    },
    Update {
        line: usize,
        source: PathBuf,
        guest_target: PathBuf,
        readonly: bool,
    },
    Remove {
        line: usize,
    },
}

impl WaylandBindingChange {
    pub fn declaration_line(&self) -> Option<usize> {
        match self {
            Self::Add { .. } => None,
            Self::Update { line, .. } | Self::Remove { line } => Some(*line),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandEndpointCatalog {
    pub sockets: Vec<HostWaylandSocket>,
    pub sources: Vec<WaylandSourceObservation>,
    pub preferred_display: Option<WaylandDisplay>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaylandSourceObservation {
    pub source: PathBuf,
    pub state: WaylandSourceState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum WaylandSourceState {
    Observed,
    Missing,
    Invalid(String),
    Unverified(String),
}

#[async_trait::async_trait]
pub(crate) trait WaylandEndpointDiscoveryPort: Send + Sync {
    async fn discover(&self, configured_sources: &[PathBuf]) -> WaylandEndpointCatalog;
}

pub(crate) struct WaylandEndpointDiscoveryService {
    port: Arc<dyn WaylandEndpointDiscoveryPort>,
}

impl WaylandEndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn WaylandEndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self, configured_sources: &[PathBuf]) -> WaylandEndpointCatalog {
        self.port.discover(configured_sources).await
    }
}
