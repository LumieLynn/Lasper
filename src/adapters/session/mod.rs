mod direct;
mod elevated;
mod pty;
pub(crate) mod terminal_attach;
mod wayland;
mod wayland_probe;

use crate::application::sessions::{SessionPort, SessionService};
use std::sync::Arc;

pub(crate) use direct::{DirectSessionAdapter, DirectTerminalPolicy};
pub(crate) use elevated::ElevatedSessionAdapter;

pub(crate) enum SessionRoute {
    Direct {
        policy: DirectTerminalPolicy,
        machine1: Option<crate::adapters::runtime::dbus::DbusBackend>,
        nspawn: crate::adapters::config::NspawnConfigStore,
    },
    Elevated {
        daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
        machine1: Option<crate::adapters::runtime::dbus::DbusBackend>,
        nspawn: crate::adapters::config::NspawnConfigStore,
    },
}

pub(crate) fn compose_session_service(route: SessionRoute) -> Arc<SessionService> {
    let port: Arc<dyn SessionPort> = match route {
        SessionRoute::Direct {
            policy,
            machine1,
            nspawn,
        } => match machine1 {
            Some(machine1) => Arc::new(DirectSessionAdapter::with_machine1(
                policy, machine1, nspawn,
            )),
            None => Arc::new(DirectSessionAdapter::new(policy)),
        },
        SessionRoute::Elevated {
            daemon,
            machine1,
            nspawn,
        } => Arc::new(ElevatedSessionAdapter::new(daemon, machine1, nspawn)),
    };
    Arc::new(SessionService::new(port))
}
