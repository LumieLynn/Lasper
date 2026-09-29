//! Typed effect requests emitted by registered configuration pages.

use super::wayland::WaylandPageAction;
use super::wayland::WaylandPageEvent;
use super::wayland::WaylandPageUpdate;
use super::x11::X11PageAction;
use super::x11::X11PageEvent;
use super::x11::X11PageUpdate;
use crate::application::configuration::ConfigurationTarget;
use crate::tui::configuration::core::page::ConfigurationPageId;
use crate::tui::StatusLevel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationPageAction {
    Wayland(WaylandPageAction),
    X11(X11PageAction),
}

#[derive(Debug)]
pub enum ConfigurationPageEvent {
    Wayland(Box<WaylandPageEvent>),
    X11(Box<X11PageEvent>),
}

pub(crate) enum ConfigurationPageUpdate {
    Wayland(WaylandPageUpdate),
    X11(Box<X11PageUpdate>),
}

impl ConfigurationPageUpdate {
    pub fn page_id(&self) -> ConfigurationPageId {
        match self {
            Self::Wayland(_) => ConfigurationPageId::Wayland,
            Self::X11(_) => ConfigurationPageId::X11,
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
            Self::Wayland(event) => (*event).detached_status(),
            Self::X11(event) => (*event).detached_status(),
        }
    }
}
