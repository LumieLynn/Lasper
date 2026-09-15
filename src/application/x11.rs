use std::sync::Arc;

use crate::domain::x11::HostX11Socket;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11EndpointCatalog {
    pub sockets: Vec<HostX11Socket>,
    pub preferred_display: Option<u16>,
    pub diagnostics: Vec<String>,
}

#[async_trait::async_trait]
pub(crate) trait X11EndpointDiscoveryPort: Send + Sync {
    async fn discover(&self) -> X11EndpointCatalog;
}

pub(crate) struct X11EndpointDiscoveryService {
    port: Arc<dyn X11EndpointDiscoveryPort>,
}

impl X11EndpointDiscoveryService {
    pub(crate) fn new(port: Arc<dyn X11EndpointDiscoveryPort>) -> Self {
        Self { port }
    }

    pub async fn discover(&self) -> X11EndpointCatalog {
        self.port.discover().await
    }
}
