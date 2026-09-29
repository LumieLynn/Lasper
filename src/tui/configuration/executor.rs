//! Effect execution boundary for registered configuration pages.
//!
//! The application shell supplies services and executes generic effects. Page
//! modules own the translation from their actions into background work.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::core::page::ConfigurationPageId;
use super::pages::{self, ConfigurationPageAction, ConfigurationPageUpdate};
use crate::application::sessions::{
    SessionService, ShellTarget, ValidatedGuestUserName, WaylandShellRequest,
};
use crate::application::x11::X11AccessService;
use crate::domain::machine::MachineName;
use crate::tui::events::AppEvent;

pub(crate) struct ConfigurationPageExecutor {
    session: Arc<SessionService>,
    x11_access: Arc<X11AccessService>,
}

impl ConfigurationPageExecutor {
    pub(crate) fn new(session: Arc<SessionService>, x11_access: Arc<X11AccessService>) -> Self {
        Self {
            session,
            x11_access,
        }
    }

    pub(crate) fn start(
        &self,
        action: ConfigurationPageAction,
        events: Option<mpsc::Sender<AppEvent>>,
    ) -> ConfigurationPageEffect {
        match action {
            ConfigurationPageAction::Wayland(action) => {
                pages::wayland::start_action(Arc::clone(&self.session), action, events)
            }
            ConfigurationPageAction::X11(action) => {
                pages::x11::start_action(Arc::clone(&self.x11_access), action, events)
            }
        }
    }
}

pub(crate) enum ConfigurationPageEffect {
    Update(ConfigurationPageUpdate),
    OpenTerminal(ConfigurationTerminalRequest),
}

pub(crate) struct ConfigurationTerminalRequest {
    page: ConfigurationPageId,
    target: ShellTarget,
    access: WaylandShellRequest,
}

impl ConfigurationTerminalRequest {
    pub(in crate::tui::configuration) fn new(
        page: ConfigurationPageId,
        target: ShellTarget,
        access: WaylandShellRequest,
    ) -> Self {
        Self {
            page,
            target,
            access,
        }
    }

    pub(crate) fn into_launch(
        self,
    ) -> (
        ConfigurationPageId,
        MachineName,
        ValidatedGuestUserName,
        WaylandShellRequest,
    ) {
        (
            self.page,
            self.target.machine().clone(),
            self.target.user().clone(),
            self.access,
        )
    }
}
