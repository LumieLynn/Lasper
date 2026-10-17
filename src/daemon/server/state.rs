//! Shared subsystems available to authenticated daemon handlers.

use crate::application::OperationRegistry;
use crate::daemon::jobs::server::DeploymentRegistry;
use crate::daemon::server::DaemonShutdown;
use crate::daemon::sessions::SessionRegistry;
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct DaemonServerState {
    pub(crate) shutdown: Arc<DaemonShutdown>,
    pub(crate) sessions: Arc<SessionRegistry>,
    pub(crate) deployments: Arc<DeploymentRegistry>,
    pub(crate) operations: Arc<OperationRegistry>,
}
