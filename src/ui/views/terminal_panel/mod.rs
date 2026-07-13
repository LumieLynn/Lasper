pub mod manager;
pub use manager::{TerminalKeyOutcome, TerminalManager, TextSelection};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

pub struct TerminalPanel;

impl TerminalPanel {
    pub fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        manager: &mut TerminalManager,
        is_focused: bool,
        resize_mode: bool,
    ) {
        let sessions = &mut manager.sessions;
        let active_idx = manager.active_idx;

        if sessions.is_empty() {
            return;
        }

        // Collect tab labels and session metadata before any mutable borrow.
        let t = crate::ui::theme::theme();
        let mut tab_spans = Vec::new();
        let session_count = sessions.len();
        for (i, s) in sessions.iter().enumerate() {
            let mut style = Style::default().fg(t.tab_inactive);
            if i == active_idx {
                style = style
                    .fg(if is_focused {
                        t.tab_active_focused
                    } else {
                        t.tab_active_unfocused
                    })
                    .add_modifier(Modifier::BOLD);
            }
            tab_spans.push(Span::styled(format!(" {} ", s.container_name), style));
            if i < session_count - 1 {
                tab_spans.push(Span::raw("-"));
            }
        }
        let tabs_line = Line::from(tab_spans);

        let session = &mut sessions[active_idx];
        let mut term = session.terminal.lock();

        let border_color = if resize_mode {
            crate::ui::panel_border_color(true, is_focused, false)
        } else if is_focused {
            if session.insert_mode {
                t.terminal_insert_border
            } else {
                t.accent
            }
        } else {
            t.border_panel_secondary
        };

        let title_suffix = if session.insert_mode {
            " [INSERT] ".to_string()
        } else if session.scroll_offset > 0 {
            let max_scroll = term.screen().row0_count();
            format!(
                " [NORMAL] (Scroll: {}/{}) ",
                session.scroll_offset.min(max_scroll),
                max_scroll
            )
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

        // Store for mouse coordinate conversion.
        manager.term_area = term_area;

        // Resize: the new Grid handles reflow internally, so no debounce needed.
        if term_area.width > 0 && term_area.height > 0 {
            let size = term.screen().size();
            if term_area.width != size.width || term_area.height != size.height {
                term.set_size(term_area.height, term_area.width);
                session.selection = TextSelection::default();
                let _ = session.pty_tx.try_send(
                    crate::nspawn::adapters::comm::pty::PtyMessage::Resize {
                        cols: term_area.width,
                        rows: term_area.height,
                    },
                );
            }
        }

        // Render directly from the live Screen — temporarily set scrollback
        // offset so cell() returns the scrolled viewport.  This does NOT
        // affect process(), so the PTY reader thread is unaffected.
        let max_scroll = term.screen().row0_count();
        term.set_scrollback(session.scroll_offset.min(max_scroll));

        // Scoped block so the &Screen borrow is released before we restore
        // scrollback to 0 via &mut term.
        let cursor_pos = {
            let screen = term.screen();
            let size = screen.size();

            // Stream-based selection: rows are row0-relative (negative = scrollback).
            // Convert to viewport-relative on the fly.
            let sel = &session.selection;
            let sel_visible = sel.active || sel.anchor != sel.extent || session.yanked;
            let scroll_off = session.scroll_offset as i32;

            for row in 0..size.height {
                if row >= term_area.height {
                    break;
                }

                // Per-row stream selection column range.
                let (sel_start, sel_end) = if sel_visible {
                    let (ar, ac) = sel.anchor;
                    let (er, ec) = sel.extent;
                    let first = ar.min(er);
                    let last = ar.max(er);
                    let rr = row as i32 - scroll_off; // this viewport row in row0-relative space

                    if rr < first || rr > last {
                        (1u16, 0u16) // empty sentinel
                    } else if ar == er {
                        (ac.min(ec), ac.max(ec))
                    } else if rr == first {
                        let is_downward = ar < er;
                        let sc = if is_downward { ac } else { ec };
                        (sc, size.width.saturating_sub(1))
                    } else if rr == last {
                        let is_downward = ar < er;
                        let ec2 = if is_downward { ec } else { ac };
                        (0u16, ec2)
                    } else {
                        (0u16, size.width.saturating_sub(1))
                    }
                } else {
                    (1u16, 0u16) // empty sentinel
                };

                for col in 0..size.width {
                    if col >= term_area.width {
                        break;
                    }

                    if let Some(cell) = screen.cell(row, col) {
                        let x = term_area.x + col;
                        let y = term_area.y + row;

                        let mut style = self.get_cell_style(cell);
                        if col >= sel_start && col <= sel_end {
                            style = style.add_modifier(Modifier::REVERSED);
                            if session.yanked {
                                style = style.add_modifier(Modifier::BOLD);
                            }
                        }
                        let c = cell.contents().chars().next().unwrap_or(' ');
                        f.buffer_mut()[(x, y)].set_char(c).set_style(style);
                    }
                }
            }

            if is_focused && session.insert_mode && session.scroll_offset == 0 {
                Some(screen.cursor_position())
            } else {
                None
            }
        }; // &Screen borrow released here

        // Restore scrollback to 0 before unlocking so the live screen is clean.
        term.set_scrollback(0);

        // One-frame yank flash — clear after rendering.
        if session.yanked {
            session.yanked = false;
            session.selection = TextSelection::default();
        }

        // Native cursor rendering (only in insert mode and not scrolled back)
        if let Some((row, col)) = cursor_pos {
            if row < term_area.height && col < term_area.width {
                f.set_cursor_position((term_area.x + col, term_area.y + row));
            }
        }
    }

    fn get_cell_style(&self, cell: &crate::term::Cell) -> Style {
        let mut style = Style::default();
        let attrs = cell.attrs();

        style = style.fg(self.map_color(attrs.fgcolor));
        style = style.bg(self.map_color(attrs.bgcolor));

        if attrs.bold() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if attrs.italic() {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if attrs.underline() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if attrs.inverse() {
            style = style.add_modifier(Modifier::REVERSED);
        }

        style
    }

    fn map_color(&self, color: crate::term::Color) -> Color {
        match color {
            crate::term::Color::Default => Color::Reset,
            crate::term::Color::Idx(i) => Color::Indexed(i),
            crate::term::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

/// Encode a crossterm `MouseEvent` as an SGR (1006) mouse escape sequence
/// suitable for forwarding to a PTY.  Coordinates must be terminal-relative
/// (0-based, top-left of terminal content area).
pub fn encode_mouse(mouse: crossterm::event::MouseEvent) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind};

    // SGR protocol uses 1-based coordinates.
    let col = mouse.column.saturating_add(1);
    let row = mouse.row.saturating_add(1);

    // Encode the button + modifier bits.
    let mut cb: u16 = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Up(_) => 3,
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 1 | 32,
        MouseEventKind::Drag(MouseButton::Right) => 2 | 32,
        MouseEventKind::Moved => 3 | 32,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };

    if mouse
        .modifiers
        .contains(crossterm::event::KeyModifiers::SHIFT)
    {
        cb |= 4;
    }
    if mouse
        .modifiers
        .contains(crossterm::event::KeyModifiers::ALT)
    {
        cb |= 8;
    }
    if mouse
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        cb |= 16;
    }

    let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
        b'm'
    } else {
        b'M'
    };

    Some(format!("\x1b[<{};{};{}{}", cb, col, row, suffix as char).into_bytes())
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
