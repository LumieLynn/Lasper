//! Typed routing envelopes for registered pages. Concrete messages belong to each page.

use super::wayland::{self, WaylandPageAction, WaylandPageEvent, WaylandPageUpdate};
use super::x11::{self, X11PageAction, X11PageEvent, X11PageUpdate};
use crate::application::configuration::ConfigurationTarget;
use crate::tui::configuration::page::ConfigurationPageId;
use crate::tui::StatusLevel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationPageAction {
    Wayland(WaylandPageAction),
    X11(X11PageAction),
}

#[derive(Debug)]
pub(crate) enum ConfigurationPageEvent {
    Wayland(Box<WaylandPageEvent>),
    X11(Box<X11PageEvent>),
}

pub(crate) enum ConfigurationPageUpdate {
    Wayland(WaylandPageUpdate),
    X11(Box<X11PageUpdate>),
}

impl ConfigurationPageUpdate {
    pub(crate) fn page_id(&self) -> ConfigurationPageId {
        match self {
            Self::Wayland(_) => wayland::PAGE_ID,
            Self::X11(_) => x11::PAGE_ID,
        }
    }
}

impl ConfigurationPageEvent {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Wayland(event) => event.label(),
            Self::X11(event) => event.label(),
        }
    }

    pub(crate) fn target(&self) -> &ConfigurationTarget {
        match self {
            Self::Wayland(event) => event.target(),
            Self::X11(event) => event.target(),
        }
    }

    pub(crate) fn into_update(self) -> ConfigurationPageUpdate {
        match self {
            Self::Wayland(event) => ConfigurationPageUpdate::Wayland((*event).into_update()),
            Self::X11(event) => ConfigurationPageUpdate::X11(Box::new((*event).into_update())),
        }
    }

    pub(crate) fn detached_status(self) -> Option<(String, StatusLevel)> {
        match self {
            Self::Wayland(event) => event.detached_status(),
            Self::X11(event) => event.detached_status(),
        }
    }
}

impl ConfigurationPageAction {
    pub(in crate::tui::configuration) fn start(
        self,
        session: &std::sync::Arc<crate::application::sessions::SessionService>,
        x11_access: &std::sync::Arc<crate::application::x11::X11AccessService>,
        events: Option<tokio::sync::mpsc::Sender<crate::tui::events::AppEvent>>,
    ) -> crate::tui::configuration::executor::ConfigurationPageEffect {
        use crate::tui::configuration::executor::PageEventSender;
        use crate::tui::events::AppEvent;
        use std::sync::Arc;

        match self {
            Self::Wayland(action) => {
                let events = events.map(|sender| {
                    PageEventSender::new(sender, |event| {
                        AppEvent::ConfigurationPage(ConfigurationPageEvent::Wayland(Box::new(
                            event,
                        )))
                    })
                });
                wayland::start_action(Arc::clone(session), action, events)
                    .map_update(ConfigurationPageUpdate::Wayland)
            }
            Self::X11(action) => {
                let events = events.map(|sender| {
                    PageEventSender::new(sender, |event| {
                        AppEvent::ConfigurationPage(ConfigurationPageEvent::X11(Box::new(event)))
                    })
                });
                x11::start_action(Arc::clone(x11_access), action, events)
                    .map_update(|update| ConfigurationPageUpdate::X11(Box::new(update)))
            }
        }
    }
}
