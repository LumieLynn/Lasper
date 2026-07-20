use crate::ui::core::{Component, EventResult};
use crate::ui::soft_wrap_text;
use crate::ui::theme;
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
}

impl ConfirmationDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
        }
    }
}

impl Component for ConfirmationDialog {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = theme::theme();
        let width = 60.min(area.width);
        let message_width = usize::from(width.saturating_sub(2)).max(1);
        let message_lines = soft_wrap_text(&self.message, message_width);
        let height = (message_lines.len() as u16)
            .saturating_add(4)
            .max(8)
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

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
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
        f.render_widget(hint_para, chunks[1]);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> EventResult {
        EventResult::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn long_messages_are_wrapped_before_rendering() {
        let message = "Delete '.oci-sha256:a3679419df184857c0d317d7cdaad6187f6c0f0b68dd2ed58becf174e28f4c1b' ?\nThis OCI backing layer may still be referenced by an mstack image.";
        let lines = soft_wrap_text(message, 30);

        assert!(lines.len() > 2);
        assert!(lines
            .iter()
            .all(|line| { unicode_width::UnicodeWidthStr::width(line.as_str()) <= 30 }));
    }

    #[test]
    fn render_keeps_long_confirmation_content_visible() {
        crate::ui::theme::init_theme(crate::ui::theme::Theme::dark());
        let mut dialog = ConfirmationDialog::new(
            "Delete Image",
            "Delete '.oci-sha256:a3679419df184857c0d317d7cdaad6187f6c0f0b68dd2ed58becf174e28f4c1b' ?\nThis OCI backing layer may still be referenced by an mstack image.",
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
        assert!(rendered.contains("This OCI backing layer may still"));
        assert!(rendered.contains("mstack image."));
    }
}
