//! Local interaction of a borrowed control. Features own its editing state,
//! and compositions supply focus, availability and the rendered rectangle.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};

use crate::tui::foundation::footer::FooterHint;

#[derive(Clone, Copy)]
pub(crate) struct ControlContext {
    pub(crate) focused: bool,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlOutcome {
    Ignored,
    Consumed,
    Changed,
    Activated,
}

pub(crate) trait Control {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext);
    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome;

    fn handle_mouse(&mut self, _mouse: MouseEvent, _area: Rect) -> ControlOutcome {
        ControlOutcome::Ignored
    }

    fn hints(&self) -> &'static [FooterHint] {
        &[]
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn captures_text_input(&self) -> bool {
        false
    }

    fn minimum_width(&self) -> u16 {
        0
    }
}
