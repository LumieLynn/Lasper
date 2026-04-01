use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use std::any::Any;

/// Messages emitted by components after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentMsg {
    /// The event was not handled by the component.
    None,
    /// The event was consumed by the component.
    Consumed,
    /// The component requests moving focus to the next element.
    FocusNext,
    /// The component requests moving focus to the previous element.
    FocusPrev,
    /// The component has been submitted (e.g., Enter pressed in an input box).
    Submit,
}

/// The base trait for all UI components in the Lasper TUI.
pub trait Component: Any {
    /// Renders the component within the given area.
    fn render(&mut self, f: &mut Frame, area: Rect);

    /// Handles a key event and returns a message indicating the result.
    fn handle_key(&mut self, key: KeyEvent) -> ComponentMsg;

    /// Sets whether the component is focused.
    fn set_focus(&mut self, focused: bool);

    /// Returns whether the component is currently focused.
    fn is_focused(&self) -> bool;

    /// Utility for downcasting to concrete types.
    fn as_any(&self) -> &dyn Any;
    
    /// Utility for downcasting to concrete types (mutable).
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
