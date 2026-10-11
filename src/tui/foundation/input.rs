//! Foreground input shared by overlays and the main workspace.

#[derive(Debug, Clone, Copy)]
pub(crate) enum UiInput {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
}
