use crate::tui::core::{AppMessage, Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

pub struct Button {
    label: String,
    focused: bool,
    enabled: bool,
    message: Box<dyn Fn() -> AppMessage>,
}

impl Button {
    pub fn new(label: impl Into<String>, message: impl Fn() -> AppMessage + 'static) -> Self {
        Self {
            label: label.into(),
            focused: false,
            enabled: true,
            message: Box::new(message),
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

impl Component for Button {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = crate::tui::theme::theme();
        let style = if !self.enabled {
            Style::default().fg(t.text_dim)
        } else if self.focused {
            Style::default()
                .fg(t.button_focused_fg)
                .bg(t.button_focused_bg)
        } else {
            Style::default().fg(t.button_unfocused_fg)
        };

        let label = format!(" {} ", self.label);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if !self.enabled {
                Style::default().fg(t.border_disabled)
            } else if self.focused {
                Style::default().fg(t.button_border_focused)
            } else {
                Style::default().fg(t.button_border_unfocused)
            });

        let p = Paragraph::new(label)
            .style(style)
            .block(block)
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(p, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if !self.enabled {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => EventResult::Message((self.message)()),
            _ => EventResult::Ignored,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::core::SessionMessage;

    #[test]
    fn disabled_button_is_not_focusable_and_emits_no_message() {
        let mut button = Button::new("Disabled", || {
            AppMessage::Session(SessionMessage::DialogSubmit)
        })
        .with_enabled(false);

        assert!(!button.is_focusable());
        assert_eq!(
            button.handle_key(KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Ignored
        );
    }
}
