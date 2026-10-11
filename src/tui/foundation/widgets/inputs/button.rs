use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

/// A button has no editing state or application callback. Its owner interprets
/// activation after the form has released its borrowed controls.
pub(crate) struct ButtonControl<'a> {
    label: &'a str,
}

const BUTTON_HINTS: &[FooterHint] = &[FooterHint::new("Enter", "activate").on_key(KeyCode::Enter)];

impl<'a> ButtonControl<'a> {
    pub(crate) fn new(label: &'a str) -> Self {
        Self { label }
    }
}

impl Control for ButtonControl<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        let t = crate::tui::foundation::theme::theme();
        let style = if !context.enabled {
            Style::default().fg(t.text_dim)
        } else if context.focused {
            Style::default()
                .fg(t.button_focused_fg)
                .bg(t.button_focused_bg)
        } else {
            Style::default().fg(t.button_unfocused_fg)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if !context.enabled {
                Style::default().fg(t.border_disabled)
            } else if context.focused {
                Style::default().fg(t.button_border_focused)
            } else {
                Style::default().fg(t.button_border_unfocused)
            });
        frame.render_widget(
            Paragraph::new(format!(" {} ", self.label))
                .style(style)
                .block(block)
                .alignment(ratatui::layout::Alignment::Center),
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => ControlOutcome::Activated,
            _ => ControlOutcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> ControlOutcome {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && area.contains((mouse.column, mouse.row).into())
        {
            ControlOutcome::Activated
        } else {
            ControlOutcome::Ignored
        }
    }

    fn hints(&self) -> &'static [FooterHint] {
        BUTTON_HINTS
    }

    fn minimum_width(&self) -> u16 {
        self.label
            .width()
            .saturating_add(4)
            .min(usize::from(u16::MAX)) as u16
    }
}
