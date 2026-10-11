use crate::tui::foundation::text::soft_wrap_text;
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crate::tui::foundation::widgets::selectors::checkbox::CheckboxControl;
use crate::tui::protocol::{Component, EventResult};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

pub struct ConfirmationDialog {
    title: String,
    message: String,
    checkbox: Option<(String, bool)>,
    checkbox_area: Rect,
    focused: bool,
    preferred_width: u16,
    hints: &'static [(&'static str, &'static str)],
}

impl ConfirmationDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            checkbox: None,
            checkbox_area: Rect::default(),
            focused: true,
            preferred_width: 60,
            hints: &[("y/Enter", "confirm"), ("n/Esc", "cancel")],
        }
    }

    pub(crate) fn with_width(mut self, width: u16) -> Self {
        self.preferred_width = width;
        self
    }

    /// Describe the parent's input bindings inside the confirmation dialog.
    pub(crate) fn with_hints(mut self, hints: &'static [(&'static str, &'static str)]) -> Self {
        self.hints = hints;
        self
    }

    pub fn with_checkbox(mut self, label: impl Into<String>, checked: bool) -> Self {
        self.checkbox = Some((label.into(), checked));
        self
    }

    pub fn checkbox_checked(&self) -> Option<bool> {
        self.checkbox.as_ref().map(|(_, checked)| *checked)
    }

    fn hint_line(&self) -> Line<'static> {
        let t = theme::theme();
        let key_style = Style::default().fg(t.key_hint_fg);
        let label_style = Style::default().fg(t.hint_fg);
        let checkbox_hint = self.checkbox.as_ref().map(|_| ("Space", "toggle"));
        let mut spans = Vec::new();
        for (key, label) in self.hints.iter().copied().chain(checkbox_hint) {
            if !spans.is_empty() {
                spans.push(Span::styled(", ", label_style));
            }
            spans.push(Span::styled(key, key_style));
            spans.push(Span::styled(format!(" {label}"), label_style));
        }
        Line::from(spans)
    }
}

impl Component for ConfirmationDialog {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.checkbox_area = Rect::default();
        let t = theme::theme();
        let width = self.preferred_width.min(area.width);
        let message_width = usize::from(width.saturating_sub(2)).max(1);
        let message_lines = soft_wrap_text(&self.message, message_width);
        let has_checkbox = self.checkbox.is_some();
        let hint_line = self.hint_line();
        let hint_height = soft_wrap_text(&hint_line.to_string(), message_width).len() as u16;
        let height = (message_lines.len() as u16)
            .saturating_add(hint_height)
            .saturating_add(if has_checkbox { 6 } else { 3 })
            .min(area.height);

        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;

        let dialog_area = Rect::new(x, y, width.min(area.width), height.min(area.height));

        f.render_widget(Clear, dialog_area);

        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.dialog_border_warn));

        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        let mut constraints = vec![Constraint::Min(1)];
        if has_checkbox {
            constraints.push(Constraint::Length(3));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(hint_height));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        let message_lines = message_lines
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        let msg_para = Paragraph::new(message_lines)
            .alignment(Alignment::Center)
            .style(Style::default().fg(t.dialog_text));

        f.render_widget(msg_para, chunks[0]);
        if let Some((label, checked)) = &mut self.checkbox {
            self.checkbox_area = chunks[1].intersection(f.area());
            CheckboxControl::new(checked, label).render(
                f,
                self.checkbox_area,
                ControlContext {
                    focused: self.focused,
                    enabled: true,
                },
            );
        }
        f.render_widget(
            Paragraph::new(hint_line)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            chunks[chunks.len() - 1],
        );
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult {
        if !self.focused || key.kind == crossterm::event::KeyEventKind::Release {
            return EventResult::Ignored;
        }
        self.checkbox
            .as_mut()
            .map(
                |(label, checked)| match CheckboxControl::new(checked, label).handle_key(key) {
                    ControlOutcome::Ignored => EventResult::Ignored,
                    _ => EventResult::Consumed,
                },
            )
            .unwrap_or(EventResult::Ignored)
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> EventResult {
        if !self.focused {
            return EventResult::Ignored;
        }
        self.checkbox
            .as_mut()
            .map_or(
                EventResult::Ignored,
                |(label, checked)| match CheckboxControl::new(checked, label)
                    .handle_mouse(mouse, self.checkbox_area)
                {
                    ControlOutcome::Ignored => EventResult::Ignored,
                    _ => EventResult::Consumed,
                },
            )
    }

    fn set_focus(&mut self, focused: bool) {
        if self.focused != focused {
            self.checkbox_area = Rect::default();
        }
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn long_messages_are_wrapped_before_rendering() {
        let message = "Delete '.oci-sha256:a3679419df184857c0d317d7cdaad6187f6c0f0b68dd2ed58becf174e28f4c1b' ?\nThis hidden image will be removed through systemd's image management path.";
        let lines = soft_wrap_text(message, 30);

        assert!(lines.len() > 2);
        assert!(lines
            .iter()
            .all(|line| { unicode_width::UnicodeWidthStr::width(line.as_str()) <= 30 }));
    }

    #[test]
    fn render_keeps_long_confirmation_content_visible() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut dialog = ConfirmationDialog::new(
            "Delete Image",
            "Delete '.oci-sha256:a3679419df184857c0d317d7cdaad6187f6c0f0b68dd2ed58becf174e28f4c1b' ?\nThis hidden image will be removed through systemd's image management path.",
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("This hidden image will be removed"));
        assert!(rendered.contains("management path."));
    }

    #[test]
    fn optional_checkbox_is_rendered_and_toggled_with_space() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut dialog = ConfirmationDialog::new("Delete Image", "Delete 'test'?")
            .with_checkbox("Remove Lasper NVIDIA state and unit drop-ins", true);
        assert_eq!(dialog.checkbox_checked(), Some(true));

        let result = dialog.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(matches!(result, EventResult::Consumed));
        assert_eq!(dialog.checkbox_checked(), Some(false));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("[ ] Remove Lasper NVIDIA state and unit drop-ins"));
    }
}
