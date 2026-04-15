use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

pub fn detail_block(_title: &str) -> Block<'static> {
    Block::default().style(Style::default().fg(Color::White))
}

pub fn empty_block(title: &str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(detail_block(title))
}
