mod direct;
mod elevated;
mod machine;
mod pty;
pub(crate) mod terminal_attach;
mod wayland;
mod wayland_probe;

use crate::application::sessions::{SessionPort, SessionService};
use std::sync::Arc;

pub(crate) use direct::{DirectSessionAdapter, DirectTerminalPolicy};
pub(crate) use elevated::ElevatedSessionAdapter;
pub(crate) use machine::MachineSessionTransport;
pub(crate) use wayland::WaylandSessionResolver;

pub(crate) enum SessionRoute {
    Direct {
        policy: DirectTerminalPolicy,
        machine: MachineSessionTransport,
        nspawn: crate::adapters::config::NspawnConfigStore,
    },
    Elevated {
        daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
    },
}

pub(crate) fn compose_session_service(route: SessionRoute) -> Arc<SessionService> {
    let port: Arc<dyn SessionPort> = match route {
        SessionRoute::Direct {
            policy,
            machine,
            nspawn,
        } => Arc::new(DirectSessionAdapter::new(policy, machine, nspawn)),
        SessionRoute::Elevated { daemon } => Arc::new(ElevatedSessionAdapter::new(daemon)),
    };
    Arc::new(SessionService::new(port))
}
