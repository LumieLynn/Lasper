//! Wayland page messages. Workspace routing is owned by the page registry.

use crate::application::configuration::ConfigurationTarget;
use crate::application::sessions::{SessionError, ShellTarget, WaylandSessionContext};
use crate::domain::wayland::HostWaylandSocket;
use crate::tui::StatusLevel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaylandPageAction {
    Check {
        generation: u64,
        target: ShellTarget,
        host_socket: HostWaylandSocket,
    },
    EnterShell {
        target: ShellTarget,
        host_socket: HostWaylandSocket,
    },
}

pub(crate) enum WaylandPageUpdate {
    TrackCheck(tokio::task::JoinHandle<()>),
    Checked {
        generation: u64,
        result: Result<WaylandSessionContext, SessionError>,
    },
}

#[derive(Debug)]
pub(crate) enum WaylandPageEvent {
    Checked {
        generation: u64,
        target: ConfigurationTarget,
        result: Result<WaylandSessionContext, SessionError>,
    },
}

impl WaylandPageEvent {
    pub(crate) fn label(&self) -> &'static str {
        "configuration-wayland-checked"
    }

    pub(crate) fn target(&self) -> &ConfigurationTarget {
        match self {
            Self::Checked { target, .. } => target,
        }
    }

    pub(crate) fn into_update(self) -> WaylandPageUpdate {
        match self {
            Self::Checked {
                generation, result, ..
            } => WaylandPageUpdate::Checked { generation, result },
        }
    }

    pub(crate) fn detached_status(self) -> Option<(String, StatusLevel)> {
        None
    }
}
