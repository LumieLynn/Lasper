//! Session delivery, guest execution, and startup-bind validation.
//!
//! Direct and elevated adapters implement the application session port. Shell
//! preparation combines display projections before execution; [`execution`]
//! selects the native provider, and [`io`] owns PTY and caller-terminal bridges.
//! [`projection`] validates existing X11/Wayland binds with shared guest probes.

mod direct;
mod elevated;
mod execution;
pub(crate) mod io;
mod projection;
mod shell;

use crate::application::sessions::{SessionPort, SessionService};
use std::sync::Arc;

pub(crate) use direct::DirectSessionAdapter;
pub(crate) use elevated::ElevatedSessionAdapter;
pub(crate) use execution::{NativeExecution, NativeSession};
pub(crate) use io::MachinePty;
pub(crate) use projection::{WaylandSessionResolver, X11SessionResolver};
pub(crate) use shell::SelectedUserShell;

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
        target: crate::application::sessions::GuestTarget,
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
        machine: NativeExecution,
        nspawn: crate::adapters::config::NspawnConfigStore,
    },
    Elevated {
        daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
    },
}

pub(crate) fn compose_session_service(route: SessionRoute) -> Arc<SessionService> {
    let port: Arc<dyn SessionPort> = match route {
        SessionRoute::Direct { machine, nspawn } => {
            Arc::new(DirectSessionAdapter::new(machine, nspawn))
        }
        SessionRoute::Elevated { daemon } => Arc::new(ElevatedSessionAdapter::new(daemon)),
    };
    Arc::new(SessionService::new(port))
}
