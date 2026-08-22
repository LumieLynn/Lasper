mod direct;
mod elevated;
mod pty;
pub(crate) mod terminal_attach;

use crate::application::sessions::{SessionPort, SessionService};
use crate::composition::ExecutionContext;
use crate::composition::PermissionLevel;
use std::sync::Arc;

pub(crate) use direct::{DirectSessionAdapter, DirectTerminalPolicy};
pub(crate) use elevated::ElevatedSessionAdapter;

pub(crate) fn compose_session_service(
    level: PermissionLevel,
    execution: &ExecutionContext,
) -> Arc<SessionService> {
    let port: Arc<dyn SessionPort> = match level {
        PermissionLevel::User => {
            Arc::new(DirectSessionAdapter::new(DirectTerminalPolicy::LoginOnly))
        }
        PermissionLevel::Root => {
            Arc::new(DirectSessionAdapter::new(DirectTerminalPolicy::Automatic))
        }
        PermissionLevel::Elevated => Arc::new(ElevatedSessionAdapter::new(
            execution
                .daemon_ref()
                .expect("validated elevated execution has a daemon")
                .clone(),
        )),
    };
    Arc::new(SessionService::new(port))
}
