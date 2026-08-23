mod direct;
mod elevated;
mod pty;
pub(crate) mod terminal_attach;

use crate::application::sessions::{SessionPort, SessionService};
use std::sync::Arc;

pub(crate) use direct::{DirectSessionAdapter, DirectTerminalPolicy};
pub(crate) use elevated::ElevatedSessionAdapter;

pub(crate) enum SessionRoute {
    Direct(DirectTerminalPolicy),
    Elevated(Arc<crate::adapters::elevated::ElevatedDaemon>),
}

pub(crate) fn compose_session_service(route: SessionRoute) -> Arc<SessionService> {
    let port: Arc<dyn SessionPort> = match route {
        SessionRoute::Direct(policy) => Arc::new(DirectSessionAdapter::new(policy)),
        SessionRoute::Elevated(daemon) => Arc::new(ElevatedSessionAdapter::new(daemon)),
    };
    Arc::new(SessionService::new(port))
}
