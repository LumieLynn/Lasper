//! Effect execution boundary for registered configuration pages.
//!
//! The application shell supplies services and executes generic effects. Page
//! modules own the translation from their actions into background work.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::pages::{ConfigurationPageAction, ConfigurationPageUpdate};
use crate::application::sessions::{
    SessionService, ShellTarget, ValidatedGuestUserName, WaylandShellRequest,
};
use crate::application::x11::X11AccessService;
use crate::domain::machine::MachineName;
use crate::tui::configuration::page::ConfigurationPageId;
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
        action.start(&self.session, &self.x11_access, events)
    }
}

pub(crate) type ConfigurationPageEffect = PageEffect<ConfigurationPageUpdate>;

pub(crate) enum PageEffect<U> {
    Update(U),
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

impl<U> PageEffect<U> {
    pub(in crate::tui::configuration) fn map_update<V>(
        self,
        map: impl FnOnce(U) -> V,
    ) -> PageEffect<V> {
        match self {
            Self::Update(update) => PageEffect::Update(map(update)),
            Self::OpenTerminal(request) => PageEffect::OpenTerminal(request),
        }
    }
}

/// The registry supplies the event conversion; a page only sends its own messages.
pub(in crate::tui::configuration) struct PageEventSender<E> {
    sender: mpsc::Sender<AppEvent>,
    wrap: fn(E) -> AppEvent,
}

impl<E> PageEventSender<E> {
    pub fn new(sender: mpsc::Sender<AppEvent>, wrap: fn(E) -> AppEvent) -> Self {
        Self { sender, wrap }
    }

    pub async fn send(&self, event: E) {
        let _ = self.sender.send((self.wrap)(event)).await;
    }
}
