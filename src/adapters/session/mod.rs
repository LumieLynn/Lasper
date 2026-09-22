//! Session adapter graph.
//!
//! `wayland` owns validation/orchestration, `wayland_probe` owns the fixed
//! probe protocol, and `machine` is the route-fixed boundary.  The latter
//! produces either a native D-Bus PTY (`runtime::dbus`) or a `machinectl`
//! command (`runtime::systemd_tools`).  `terminal_attach` is only the default
//! login/namespace selector; `terminal_io` owns the process-level I/O bridges.

mod direct;
mod elevated;
mod instance;
mod machine;
mod pty;
pub(crate) mod terminal_attach;
pub(crate) mod terminal_io;
mod wayland;
mod wayland_probe;
mod x11;
mod x11_probe;

use crate::application::sessions::{SessionPort, SessionService};
use std::sync::Arc;

pub(crate) use direct::{DirectSessionAdapter, DirectTerminalPolicy};
pub(crate) use elevated::ElevatedSessionAdapter;
pub(crate) use machine::{
    MachinePty, MachineSessionOpening, MachineSessionRequest, MachineSessionTransport,
    MachineShellEnvironment, MachineShellRequest,
};
pub(crate) use wayland::WaylandSessionResolver;
pub(crate) use wayland_probe::WaylandProbeRequest;
pub(crate) use x11::X11SessionResolver;
pub(crate) use x11_probe::X11ProjectionProbeRequest;

/// Adapts the selected session transport to the narrow projection capability
/// consumed by X11 access management. Keeping this adapter in composition
/// prevents the X11 application service from depending on SessionService.
pub(crate) struct X11ProjectionAdapter {
    session: Arc<SessionService>,
}

impl X11ProjectionAdapter {
    pub(crate) fn new(session: Arc<SessionService>) -> Self {
        Self { session }
    }
}

#[async_trait::async_trait]
impl crate::application::x11::X11ProjectionPort for X11ProjectionAdapter {
    async fn probe(
        &self,
        target: crate::application::sessions::ShellTarget,
        host_socket: crate::domain::x11::HostX11Socket,
    ) -> Result<
        crate::application::sessions::X11ProjectionContext,
        crate::application::sessions::SessionError,
    > {
        self.session.test_x11_projection(target, host_socket).await
    }
}

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
