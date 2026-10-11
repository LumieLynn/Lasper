//! Shared component interaction contract for the TUI.
//!
//! Feature actions are defined by their owners; this protocol carries them
//! between feature views and the coordinating application. Foundation controls
//! return local outcomes and do not depend on application messages.

pub(crate) mod events;
pub(crate) use crate::tui::features::wizard::messages::WizardMessage;
pub(crate) use crate::tui::features::workspace::inspector::messages::ContainerMessage;
pub(crate) use crate::tui::features::workspace::resources::messages::ListMessage;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum AppMessage {
    Wizard(WizardMessage),
    Container(ContainerMessage),
    List(ListMessage),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum EventResult {
    Ignored,             // Not handled, bubble up
    Consumed,            // Handled, no further action needed
    Message(AppMessage), // Handled, produced a business message
}

pub trait Component {
    fn render(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect);
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult;

    /// Register this component's current input hints. Parents delegate to
    /// focused controls or a nested dialog before adding their own actions.
    fn register_footer_hints(&self, _footer: &mut crate::tui::foundation::footer::FooterRegistry) {}

    /// Paint an overlay without allowing a covered component to claim the
    /// cursor. Its own focus selection is restored before routing more input.
    fn render_layer(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        foreground: bool,
    ) {
        self.set_focus(foreground);
        self.render(f, area);
        if !foreground {
            self.set_focus(true);
        }
    }

    /// Handle a mouse event in the component's last rendered area.  Widgets
    /// that do not expose mouse interaction keep the default no-op; parents
    /// still consume the event when they are modal so it cannot bubble into
    /// the background UI.
    fn handle_mouse(&mut self, _mouse: crossterm::event::MouseEvent) -> EventResult {
        EventResult::Ignored
    }

    fn set_focus(&mut self, _focused: bool) {}
    fn is_focused(&self) -> bool {
        false
    }
    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}
