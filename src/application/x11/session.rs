//! X11 session selection and prepared-session contracts.
//!
//! This module connects a selected host endpoint to a verified guest
//! projection and an authorization result. ACL observation and lifecycle
//! reconciliation remain owned by the parent access service.

use crate::application::sessions::X11SessionContext;
use crate::domain::x11::HostX11Socket;

use super::{X11AccessCheck, X11AuthorizationDisposition, X11EndpointCatalog};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11SessionSelection {
    Current,
    Display(u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11SessionPreparation {
    pub(super) context: X11SessionContext,
    pub(super) check: X11AccessCheck,
    pub(super) disposition: X11AuthorizationDisposition,
}

impl X11SessionPreparation {
    pub fn context(&self) -> &X11SessionContext {
        &self.context
    }

    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11AuthorizationDisposition {
        &self.disposition
    }

    pub fn into_context(self) -> X11SessionContext {
        self.context
    }
}

pub(super) fn select_session_endpoints(
    catalog: X11EndpointCatalog,
    selection: X11SessionSelection,
) -> Result<(u16, Vec<HostX11Socket>), String> {
    let display = match selection {
        X11SessionSelection::Current => catalog.preferred_display.ok_or_else(|| {
            "the current DISPLAY does not identify a local X11 server; select one explicitly with --with-x11=:N"
                .to_owned()
        })?,
        X11SessionSelection::Display(display) => display,
    };
    let mut candidates = catalog
        .sockets
        .iter()
        .filter(|socket| socket.display() == display)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(HostX11Socket::alternate);
    if !candidates.is_empty() {
        return Ok((display, candidates));
    }

    let mut available = catalog
        .sockets
        .iter()
        .map(HostX11Socket::display)
        .collect::<Vec<_>>();
    available.sort_unstable();
    available.dedup();
    let available = if available.is_empty() {
        "none".to_owned()
    } else {
        available
            .into_iter()
            .map(|display| format!(":{display}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let diagnostic = catalog
        .diagnostics
        .first()
        .map(|message| format!("; discovery: {message}"))
        .unwrap_or_default();
    Err(format!(
        "X11 display :{display} was not discovered (available: {available}){diagnostic}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_selection_requires_a_preferred_display() {
        let error =
            select_session_endpoints(X11EndpointCatalog::default(), X11SessionSelection::Current)
                .unwrap_err();
        assert!(error.contains("current DISPLAY"));
    }
}
