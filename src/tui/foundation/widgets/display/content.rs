//! Non-interactive content participates in a form's layout without joining its
//! focus order. The feature constructs its text and styling.
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, widgets::Widget, Frame};

pub(crate) struct ContentControl<W> {
    widget: W,
}

impl<W> ContentControl<W> {
    pub(crate) fn new(widget: W) -> Self {
        Self { widget }
    }
}

impl<W: Widget + Clone> Control for ContentControl<W> {
    fn render(&mut self, frame: &mut Frame, area: Rect, _: ControlContext) {
        frame.render_widget(self.widget.clone(), area);
    }
    fn handle_key(&mut self, _: KeyEvent) -> ControlOutcome {
        ControlOutcome::Ignored
    }
    fn is_focusable(&self) -> bool {
        false
    }
}
