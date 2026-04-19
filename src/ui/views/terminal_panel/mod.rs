use ratatui::{
    layout::{Rect, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};
use crate::app::TerminalSession;

pub struct TerminalPanel;

impl TerminalPanel {
    pub fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        sessions: &[TerminalSession],
        active_idx: usize,
        is_focused: bool,
    ) {
        if sessions.is_empty() {
            return;
        }

        let session = &sessions[active_idx];
        let mut term = session.terminal.lock();
        
        // Tab generation in DetailPanel style
        let mut spans = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            let mut style = Style::default().fg(Color::DarkGray);
            if i == active_idx {
                style = style
                    .fg(if is_focused { Color::Yellow } else { Color::White })
                    .add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(format!(" {} ", s.container_name), style));
            if i < sessions.len() - 1 {
                spans.push(Span::raw("-"));
            }
        }
        let tabs_line = Line::from(spans);

        let border_color = if is_focused {
            if session.insert_mode { Color::Green } else { Color::Cyan }
        } else {
            Color::DarkGray
        };

        let title_suffix = if session.insert_mode { 
            " [INSERT] ".to_string()
        } else if session.scroll_offset > 0 {
            let mut screen_probe = term.screen().clone();
            screen_probe.set_scrollback(usize::MAX);
            let max_scroll = screen_probe.scrollback();
            format!(" [NORMAL] (Scroll: {}/{}) ", session.scroll_offset.min(max_scroll), max_scroll)
        } else {
            " [NORMAL] ".to_string()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(tabs_line)
            .title_bottom(Line::from(vec![Span::raw(title_suffix)]).alignment(Alignment::Right));

        let term_area = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Dynamic resizing
        if term_area.width > 0 && term_area.height > 0 {
            let (rows, cols) = term.screen().size();
            if term_area.width != cols || term_area.height != rows {
                term.set_size(term_area.height, term_area.width);
                let _ = session.pty_tx.try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Resize { 
                    cols: term_area.width, 
                    rows: term_area.height 
                });
            }
        }

        // Clone the screen so we don't mutate the live parser state, then shift the viewport
        let mut screen = term.screen().clone();
        
        // Use the probe hack to find the actual history limit for clamping
        let mut screen_probe = screen.clone();
        screen_probe.set_scrollback(usize::MAX);
        let max_scroll = screen_probe.scrollback();
        
        screen.set_scrollback(session.scroll_offset.min(max_scroll));
        let (rows, cols) = screen.size();

        for row in 0..rows {
            if row >= term_area.height { break; }
            for col in 0..cols {
                if col >= term_area.width { break; }
                
                if let Some(cell) = screen.cell(row, col) {
                    let x = term_area.x + col;
                    let y = term_area.y + row;
                    
                    let style = self.get_cell_style(cell);
                    let c = cell.contents().chars().next().unwrap_or(' ');
                    f.buffer_mut()[(x, y)].set_char(c).set_style(style);
                }
            }
        }

        // Native cursor rendering (only in insert mode and not scrolled back)
        if is_focused && session.insert_mode && session.scroll_offset == 0 {
            let (row, col) = screen.cursor_position();
            if row < term_area.height && col < term_area.width {
                f.set_cursor_position((term_area.x + col, term_area.y + row));
            }
        }
    }

    fn get_cell_style(&self, cell: &vt100::Cell) -> Style {
        let mut style = Style::default();
        
        style = style.fg(self.map_color(cell.fgcolor()));
        style = style.bg(self.map_color(cell.bgcolor()));

        if cell.bold() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if cell.italic() {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if cell.underline() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if cell.inverse() {
            style = style.add_modifier(Modifier::REVERSED);
        }

        style
    }

    fn map_color(&self, color: vt100::Color) -> Color {
        match color {
            vt100::Color::Default => Color::Reset,
            vt100::Color::Idx(i) => Color::Indexed(i),
            vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

pub fn encode_key(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyModifiers;
    match key.code {
        crossterm::event::KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'a'..='z' => vec![(c as u8) - b'a' + 1],
                    '[' => vec![27],
                    '\\' => vec![28],
                    ']' => vec![29],
                    '^' => vec![30],
                    '_' => vec![31],
                    _ => vec![c as u8],
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                vec![27, c as u8]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        crossterm::event::KeyCode::Enter => vec![b'\r'],
        crossterm::event::KeyCode::Esc => vec![27],
        crossterm::event::KeyCode::Backspace => vec![127],
        crossterm::event::KeyCode::Tab => vec![9],
        crossterm::event::KeyCode::Up => vec![27, b'[', b'A'],
        crossterm::event::KeyCode::Down => vec![27, b'[', b'B'],
        crossterm::event::KeyCode::Right => vec![27, b'[', b'C'],
        crossterm::event::KeyCode::Left => vec![27, b'[', b'D'],
        crossterm::event::KeyCode::Home => vec![27, b'[', b'H'],
        crossterm::event::KeyCode::End => vec![27, b'[', b'F'],
        crossterm::event::KeyCode::PageUp => vec![27, b'[', b'5', b'~'],
        crossterm::event::KeyCode::PageDown => vec![27, b'[', b'6', b'~'],
        crossterm::event::KeyCode::Delete => vec![27, b'[', b'3', b'~'],
        _ => vec![],
    }
}
