use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

use crate::ui::theme;

pub fn detail_block(_title: &str) -> Block<'static> {
    Block::default().style(Style::default().fg(theme::theme().text_primary))
}

pub fn empty_block(title: &str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(theme::theme().text_secondary),
        )),
    ])
    .block(detail_block(title))
}
