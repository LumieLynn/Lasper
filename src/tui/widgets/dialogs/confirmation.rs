use crate::tui::core::{Component, EventResult};
use crate::tui::soft_wrap_text;
use crate::tui::theme;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

pub struct ConfirmationDialog {
    title: String,
    message: String,
    checkbox: Option<Checkbox>,
}

impl ConfirmationDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            checkbox: None,
        }
    }

    pub fn with_checkbox(mut self, label: impl Into<String>, checked: bool) -> Self {
        let mut checkbox = Checkbox::new(label, checked);
        checkbox.set_focus(true);
        self.checkbox = Some(checkbox);
        self
    }

    pub fn checkbox_checked(&self) -> Option<bool> {
        self.checkbox.as_ref().map(Checkbox::checked)
    }
}

impl Component for ConfirmationDialog {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = theme::theme();
        let width = 60.min(area.width);
        let message_width = usize::from(width.saturating_sub(2)).max(1);
        let message_lines = soft_wrap_text(&self.message, message_width);
        let has_checkbox = self.checkbox.is_some();
        let height = (message_lines.len() as u16)
            .saturating_add(if has_checkbox { 7 } else { 4 })
            .max(if has_checkbox { 9 } else { 8 })
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
        constraints.push(Constraint::Length(2));
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

        let hint = Line::from(vec![
            Span::styled(
                " [y] ",
                Style::default()
                    .fg(t.confirm_hint)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Confirm   "),
            Span::styled(
                " [n/Esc] ",
                Style::default()
                    .fg(t.cancel_hint)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Cancel"),
        ]);

        let hint_para = Paragraph::new(hint).alignment(Alignment::Center);

        f.render_widget(msg_para, chunks[0]);
        if let Some(checkbox) = &mut self.checkbox {
            checkbox.render(f, chunks[1]);
        }
        f.render_widget(hint_para, chunks[chunks.len() - 1]);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult {
        self.checkbox
            .as_mut()
            .map(|checkbox| checkbox.handle_key(key))
            .unwrap_or(EventResult::Ignored)
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
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
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
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
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
