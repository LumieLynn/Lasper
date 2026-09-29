//! Configuration workspace presentation.

mod core;
mod executor;
mod pages;

pub(crate) use core::{ConfigurationAction, ConfigurationView};
pub(crate) use executor::{
    ConfigurationPageEffect, ConfigurationPageExecutor, ConfigurationTerminalRequest,
};
#[cfg(test)]
pub(crate) use pages::wayland::{WaylandPageAction, WaylandPageEvent};
#[cfg(test)]
pub(crate) use pages::x11::{X11PageAction, X11PageEvent};
#[cfg(test)]
pub(crate) use pages::ConfigurationPageAction;
pub(crate) use pages::ConfigurationPageEvent;
