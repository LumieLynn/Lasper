use crate::ui::core::{Component, EventResult};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
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
        let width = 50;
        let height = 8;
        
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        
        let dialog_area = Rect::new(x, y, width.min(area.width), height.min(area.height));
        
        f.render_widget(Clear, dialog_area);
        
        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
            
        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(inner);
            
        let msg_para = Paragraph::new(self.message.as_str())
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::White));
            
        let hint = Line::from(vec![
            Span::styled(" [y] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("Confirm   "),
            Span::styled(" [n/Esc] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw("Cancel"),
        ]);
        
        let hint_para = Paragraph::new(hint)
            .alignment(Alignment::Center);
            
        f.render_widget(msg_para, chunks[0]);
        f.render_widget(hint_para, chunks[1]);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> EventResult {
        EventResult::Ignored
    }
}
