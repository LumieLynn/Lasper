use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

const CHECKBOX_HINTS: &[FooterHint] =
    &[FooterHint::new("Space", "toggle").on_key(KeyCode::Char(' '))];

pub(crate) struct CheckboxControl<'a> {
    checked: &'a mut bool,
    label: &'a str,
}

impl<'a> CheckboxControl<'a> {
    pub(crate) fn new(checked: &'a mut bool, label: &'a str) -> Self {
        Self { checked, label }
    }
}

impl Control for CheckboxControl<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        let t = crate::tui::foundation::theme::theme();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default().fg(crate::tui::foundation::theme::widget_border_color(
                    context.focused,
                    context.enabled,
                )),
            );
        let symbol = if *self.checked { "[x]" } else { "[ ]" };
        let style = if context.enabled {
            Style::default()
        } else {
            Style::default()
                .fg(t.border_disabled)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(format!("{symbol} {}", self.label))
                .style(style)
                .block(block),
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        if key.code == KeyCode::Char(' ') {
            *self.checked = !*self.checked;
            ControlOutcome::Changed
        } else {
            ControlOutcome::Ignored
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> ControlOutcome {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && area.contains((mouse.column, mouse.row).into())
        {
            *self.checked = !*self.checked;
            ControlOutcome::Changed
        } else {
            ControlOutcome::Ignored
        }
    }

    fn hints(&self) -> &'static [FooterHint] {
        CHECKBOX_HINTS
    }
}
