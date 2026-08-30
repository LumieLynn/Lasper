use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Block, BorderType, Borders, Cell, Clear, Row, Table},
    Frame,
};

use crate::tui::core::{Component, EventResult};
use crate::tui::{centered_rect, theme};

pub struct HelpOverlay;

impl HelpOverlay {
    pub fn new() -> Self {
        Self
    }
}

impl Component for HelpOverlay {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = theme::theme();
        let area = centered_rect(60, 85, area);
        f.render_widget(Clear, area);

        let header_style = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
        let key_style = Style::default().fg(t.help_key);
        let desc_style = Style::default().fg(t.text_primary);
        let close_style = Style::default().fg(t.help_close_hint);

        let widths = [Constraint::Percentage(35), Constraint::Percentage(65)];

        let rows = vec![
            category_row(" Navigation ", header_style),
            key_row("j / ↓", "Select next item", key_style, desc_style),
            key_row("k / ↑", "Select previous item", key_style, desc_style),
            key_row("Tab / Shift+Tab", "Switch panels", key_style, desc_style),
            key_row(
                "[ / ] or Alt+1/2",
                "Switch image tab",
                key_style,
                desc_style,
            ),
            spacer_row(),
            category_row(" Detail Panes ", header_style),
            key_row("Alt+1..5", "Switch detail pane", key_style, desc_style),
            key_row("[ / ]", "Cycle detail panes", key_style, desc_style),
            key_row("↑/↓ / j/k", "Scroll in detail pane", key_style, desc_style),
            spacer_row(),
            category_row(" Terminal ", header_style),
            key_row("Alt+1..9", "Switch terminal tab", key_style, desc_style),
            key_row("[ / ]", "Cycle terminal tabs", key_style, desc_style),
            spacer_row(),
            category_row(" Actions [root] ", header_style),
            key_row("s", "Start selected image", key_style, desc_style),
            key_row("S", "Poweroff selected machine", key_style, desc_style),
            key_row("D", "Delete focused image", key_style, desc_style),
            key_row("x / Enter", "Open resource actions", key_style, desc_style),
            key_row(
                "n / a",
                "New container / Import wizard",
                key_style,
                desc_style,
            ),
            spacer_row(),
            category_row(" General ", header_style),
            key_row("r", "Refresh list", key_style, desc_style),
            key_row("R", "Enter resize mode", key_style, desc_style),
            key_row("?", "Toggle help", key_style, desc_style),
            key_row("q", "Quit", key_style, desc_style),
            spacer_row(),
            Row::new(vec![
                Cell::from(Span::styled(" Press any key to close", close_style)),
                Cell::from(""),
            ]),
        ];

        let table = Table::new(rows, widths).block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(t.help_border)),
        );

        f.render_widget(table, area);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Consumed
    }
}

fn category_row(title: &'static str, style: Style) -> Row<'static> {
    Row::new(vec![Cell::from(Span::styled(title, style)), Cell::from("")])
}

fn key_row(
    key: &'static str,
    desc: &'static str,
    key_style: Style,
    desc_style: Style,
) -> Row<'static> {
    Row::new(vec![
        Cell::from(Span::styled(format!("  {}", key), key_style)),
        Cell::from(Span::styled(desc, desc_style)),
    ])
}

fn spacer_row() -> Row<'static> {
    Row::new(vec![Cell::from(""), Cell::from("")])
}
