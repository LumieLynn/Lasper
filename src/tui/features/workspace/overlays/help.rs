use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::scrolling::{register_scroll_hints, ScrollState};
use crate::tui::foundation::widgets::display::scrollbar::VerticalScrollbar;
use crate::tui::foundation::{geometry::centered_rect, text::soft_wrap_text, theme};
use crate::tui::protocol::{Component, EventResult};

enum HelpEntry {
    Category(&'static str),
    Binding(&'static str, &'static str),
    Spacer,
}

const HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry::Category("Navigation"),
    HelpEntry::Binding("j / ↓", "Select next item"),
    HelpEntry::Binding("k / ↑", "Select previous item"),
    HelpEntry::Binding("Tab / ⇧Tab", "Switch panels"),
    HelpEntry::Binding("Space, then e", "Configure resource"),
    HelpEntry::Binding("[ / ] or Alt+1/2", "Switch image tab"),
    HelpEntry::Spacer,
    HelpEntry::Category("Detail Panes"),
    HelpEntry::Binding("Alt+1..5", "Switch detail pane"),
    HelpEntry::Binding("[ / ]", "Cycle detail panes"),
    HelpEntry::Binding("↑/↓ / j/k", "Scroll in detail pane"),
    HelpEntry::Spacer,
    HelpEntry::Category("Terminal"),
    HelpEntry::Binding("t", "Open selected-user shell"),
    HelpEntry::Binding("Space, then t/l", "Shell or login terminal"),
    HelpEntry::Binding("Alt+1..9", "Switch terminal tab"),
    HelpEntry::Binding("[ / ]", "Cycle terminal tabs"),
    HelpEntry::Spacer,
    HelpEntry::Category("Actions [root]"),
    HelpEntry::Binding("s", "Start selected image"),
    HelpEntry::Binding("S", "Poweroff selected machine"),
    HelpEntry::Binding("D", "Delete focused image"),
    HelpEntry::Binding("x / Enter", "Open resource actions"),
    HelpEntry::Binding("n / a", "New container / Import wizard"),
    HelpEntry::Spacer,
    HelpEntry::Category("General"),
    HelpEntry::Binding("r", "Refresh list"),
    HelpEntry::Binding("R", "Enter resize mode"),
    HelpEntry::Binding("?", "Toggle help"),
    HelpEntry::Binding("q", "Quit"),
];

/// Help owns its scroll position for exactly as long as the overlay is open.
#[derive(Default)]
pub struct HelpOverlay {
    area: Rect,
    viewport: Rect,
    scroll: ScrollState,
    scrollbar: VerticalScrollbar,
}

impl HelpOverlay {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Component for HelpOverlay {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        footer.register(
            FooterHint::new("Any other key", "close help")
                .primary()
                .on_key(KeyCode::Esc),
        );
        if self.scroll.max_offset() > 0 {
            register_scroll_hints(footer);
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::theme();
        self.area = centered_rect(60, 85, area);
        frame.render_widget(Clear, self.area);
        let block = Block::default()
            .title(" Help ")
            .title_style(Style::default().fg(t.help_title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.help_border));
        let inner = block.inner(self.area);
        frame.render_widget(block, self.area);

        self.viewport = inner;
        let lines = help_lines(self.viewport.width);
        self.scroll
            .update(lines.len(), usize::from(self.viewport.height));
        let visible = lines
            .into_iter()
            .skip(self.scroll.offset())
            .take(usize::from(self.viewport.height))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), self.viewport);
        self.scrollbar.render(
            frame,
            Rect::new(
                self.viewport.right(),
                self.viewport.y,
                1,
                self.viewport.height,
            ),
            &self.scroll,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Unhandled keys dismiss help in its owner and never reach the workspace.
        self.scroll.handle_key(key)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if self.scrollbar.handle_mouse(mouse, &mut self.scroll) == EventResult::Consumed {
            return EventResult::Consumed;
        }
        if self.area.contains(Position::new(mouse.column, mouse.row)) {
            self.scroll.handle_mouse(mouse.kind);
        }
        EventResult::Consumed
    }
}

/// Wrap before slicing the viewport so scrolling counts terminal rows and
/// can reach every continuation, even when only one row fits on screen.
fn help_lines(width: u16) -> Vec<Line<'static>> {
    let t = theme::theme();
    let width = usize::from(width.max(1));
    let header_style = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(t.help_key);
    let description_style = Style::default().fg(t.text_primary);
    let key_width = width * 35 / 100;
    let description_width = width.saturating_sub(key_width + 1);
    let mut lines = Vec::new();
    for entry in HELP_ENTRIES {
        match entry {
            HelpEntry::Category(title) => {
                lines.extend(
                    soft_wrap_text(title, width)
                        .into_iter()
                        .map(|text| Line::styled(text, header_style)),
                );
            }
            HelpEntry::Binding(key, description) if width >= 40 => {
                let keys = soft_wrap_text(key, key_width.saturating_sub(2));
                let descriptions = soft_wrap_text(description, description_width);
                for index in 0..keys.len().max(descriptions.len()) {
                    let key = keys.get(index).map_or("", String::as_str);
                    let description = descriptions.get(index).map_or("", String::as_str);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {key}"), key_style),
                        Span::raw(" ".repeat(key_width.saturating_sub(2 + key.width()) + 1)),
                        Span::styled(description.to_owned(), description_style),
                    ]));
                }
            }
            HelpEntry::Binding(key, description) => {
                lines.extend(
                    soft_wrap_text(key, width)
                        .into_iter()
                        .map(|text| Line::styled(text, key_style)),
                );
                lines.extend(
                    soft_wrap_text(description, width)
                        .into_iter()
                        .map(|text| Line::styled(text, description_style)),
                );
            }
            HelpEntry::Spacer => lines.push(Line::default()),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(help: &mut HelpOverlay, width: u16, height: u16) -> String {
        theme::init_theme(theme::Theme::dark());
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| help.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn growing_the_viewport_releases_scroll() {
        let mut help = HelpOverlay::new();
        let top = render(&mut help, 120, 20);
        assert!(top.contains("Navigation"));
        assert!(top.contains('↑') && top.contains('↓'));
        help.handle_mouse(MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: help.viewport.right(),
            row: help.viewport.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(help.scroll.offset(), 1);
        help.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let bottom = render(&mut help, 120, 20);
        assert!(bottom.contains("Toggle help"));
        assert!(help.scroll.offset() > 0);

        let wide = render(&mut help, 180, 70);
        assert_eq!(help.scroll.offset(), 0);
        assert_eq!(help.scroll.max_offset(), 0);
        assert!(wide.contains("Navigation") && wide.contains("Toggle help"));
        assert_eq!(
            wide.lines()
                .nth(usize::from(help.viewport.y))
                .unwrap()
                .chars()
                .nth(usize::from(help.viewport.right()))
                .unwrap(),
            '│',
        );

        // A nearly empty viewport still clamps offsets and can recover when
        // resized; none of its continuations are retained as permanent state.
        for (width, height) in [(30, 8), (1, 1), (120, 20)] {
            render(&mut help, width, height);
        }
        assert!(help.scroll.offset() <= help.scroll.max_offset());
    }
}
