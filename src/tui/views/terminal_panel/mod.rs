pub mod manager;
use crate::tui::views::title_tabs::bordered_title_tab_hitboxes;
pub use manager::{TerminalInputStatus, TerminalKeyOutcome, TerminalManager, TextSelection};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};
use unicode_width::UnicodeWidthStr;

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
        if manager.sessions.is_empty() {
            manager.tab_hitboxes.clear();
            manager.term_area = Rect::default();
            return;
        }
        let active_idx = manager.active_idx;

        let tab_widths = manager
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| (index, session.machine_name.width().saturating_add(2)))
            .collect::<Vec<_>>();
        manager.tab_hitboxes = bordered_title_tab_hitboxes(area, Alignment::Left, &tab_widths, 1);

        // Collect tab labels and session metadata before any mutable borrow.
        let t = crate::tui::theme::theme();
        let mut tab_spans = Vec::new();
        let session_count = manager.sessions.len();
        for (i, s) in manager.sessions.iter().enumerate() {
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
            tab_spans.push(Span::styled(format!(" {} ", s.machine_name), style));
            if i < session_count - 1 {
                tab_spans.push(Span::raw("-"));
            }
        }
        let tabs_line = Line::from(tab_spans);

        let session = &mut manager.sessions[active_idx];
        // Clone the Arc before locking so the guard does not hold an
        // immutable borrow of the whole session while resize state changes.
        let terminal = session.terminal.clone();
        let mut term = terminal.lock();

        let border_color = if resize_mode {
            crate::tui::panel_border_color(true, is_focused, false)
        } else if is_focused {
            if session.is_insert_mode() {
                t.terminal_insert_border
            } else {
                t.accent
            }
        } else {
            t.border_panel_secondary
        };

        let title_suffix = match session.handle.lifecycle() {
            crate::domain::session::SessionLifecycle::Running if session.insert_mode => {
                " [INSERT] ".to_string()
            }
            crate::domain::session::SessionLifecycle::Running if session.scroll_offset > 0 => {
                let max_scroll = term.screen().row0_count();
                format!(
                    " [NORMAL] (Scroll: {}/{}) ",
                    session.scroll_offset.min(max_scroll),
                    max_scroll
                )
            }
            crate::domain::session::SessionLifecycle::Running => " [NORMAL] ".to_string(),
            crate::domain::session::SessionLifecycle::Exited { code, .. } => match code {
                Some(code) => format!(" [EXIT {code}] "),
                None => " [EXITED] ".to_string(),
            },
            crate::domain::session::SessionLifecycle::Failed(_) => " [FAILED] ".to_string(),
            crate::domain::session::SessionLifecycle::Closed => " [CLOSED] ".to_string(),
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

        // Resize the local parser immediately, and keep retrying the latest
        // PTY request until the bounded writer queue accepts it.
        if term_area.width > 0 && term_area.height > 0 {
            session.request_resize(&mut term, term_area.width, term_area.height);
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

            if is_focused
                && session.is_insert_mode()
                && session.scroll_offset == 0
                && !screen.hide_cursor()
            {
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

    fn get_cell_style(&self, cell: &crate::tui::term::Cell) -> Style {
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

    fn map_color(&self, color: crate::tui::term::Color) -> Color {
        match color {
            crate::tui::term::Color::Default => Color::Reset,
            crate::tui::term::Color::Idx(i) => Color::Indexed(i),
            crate::tui::term::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

/// Encode a mouse event using the protocol selected by the terminal.
/// Coordinates are terminal-relative (0-based, top-left of the content area).
#[allow(dead_code)]
pub fn encode_mouse(mouse: crossterm::event::MouseEvent) -> Option<Vec<u8>> {
    encode_mouse_for_protocol(
        mouse,
        crate::tui::term::screen::MouseProtocolMode::AnyMotion,
        crate::tui::term::screen::MouseProtocolEncoding::Sgr,
    )
}

pub fn encode_mouse_for_protocol(
    mouse: crossterm::event::MouseEvent,
    mode: crate::tui::term::screen::MouseProtocolMode,
    encoding: crate::tui::term::screen::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind};

    if mode == crate::tui::term::screen::MouseProtocolMode::None
        || !mouse_kind_allowed(mouse.kind, mode)
    {
        return None;
    }

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

    match encoding {
        crate::tui::term::screen::MouseProtocolEncoding::Sgr => {
            // SGR protocol uses 1-based coordinates and supports large values.
            let col = mouse.column.saturating_add(1);
            let row = mouse.row.saturating_add(1);
            let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{};{};{}{}", cb, col, row, suffix).into_bytes())
        }
        crate::tui::term::screen::MouseProtocolEncoding::Default
        | crate::tui::term::screen::MouseProtocolEncoding::Utf8 => {
            // Legacy encodings use 1-based coordinates offset by 32.  The
            // default byte form is limited to 223 cells; UTF-8 extends it.
            let col = mouse.column.saturating_add(33);
            let row = mouse.row.saturating_add(33);
            if encoding == crate::tui::term::screen::MouseProtocolEncoding::Default
                && (col > 255 || row > 255)
            {
                return None;
            }

            let mut sequence = vec![b'\x1b', b'[', b'M'];
            if encoding == crate::tui::term::screen::MouseProtocolEncoding::Default {
                sequence.extend([cb.saturating_add(32) as u8, col as u8, row as u8]);
            } else {
                append_utf8_mouse_value(&mut sequence, cb.saturating_add(32));
                append_utf8_mouse_value(&mut sequence, col);
                append_utf8_mouse_value(&mut sequence, row);
            }
            Some(sequence)
        }
    }
}

fn mouse_kind_allowed(
    kind: crossterm::event::MouseEventKind,
    mode: crate::tui::term::screen::MouseProtocolMode,
) -> bool {
    use crossterm::event::MouseEventKind;
    match mode {
        crate::tui::term::screen::MouseProtocolMode::None => false,
        crate::tui::term::screen::MouseProtocolMode::Press => {
            matches!(
                kind,
                MouseEventKind::Down(_)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
        }
        crate::tui::term::screen::MouseProtocolMode::PressRelease => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        crate::tui::term::screen::MouseProtocolMode::ButtonMotion => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        crate::tui::term::screen::MouseProtocolMode::AnyMotion => true,
    }
}

fn append_utf8_mouse_value(sequence: &mut Vec<u8>, value: u16) {
    if let Some(character) = char::from_u32(u32::from(value)) {
        let mut encoded = [0u8; 4];
        sequence.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }
}

#[allow(dead_code)]
pub fn encode_key(key: crossterm::event::KeyEvent) -> Vec<u8> {
    encode_key_for_mode(key, false)
}

pub fn encode_key_for_mode(key: crossterm::event::KeyEvent, application_cursor: bool) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let modifiers = key.modifiers;
    let modifier_param = xterm_modifier_param(modifiers);

    match key.code {
        KeyCode::Char(c) => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                match c.to_ascii_lowercase() {
                    'a'..='z' => vec![(c.to_ascii_lowercase() as u8) - b'a' + 1],
                    '[' => vec![27],
                    '\\' => vec![28],
                    ']' => vec![29],
                    '^' => vec![30],
                    '_' => vec![31],
                    _ => c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec(),
                }
            } else if modifiers.contains(KeyModifiers::ALT) {
                let mut bytes = vec![27];
                let mut encoded = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
                bytes
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Esc => vec![27],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![9],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => encode_cursor_key(b'A', application_cursor, modifier_param),
        KeyCode::Down => encode_cursor_key(b'B', application_cursor, modifier_param),
        KeyCode::Right => encode_cursor_key(b'C', application_cursor, modifier_param),
        KeyCode::Left => encode_cursor_key(b'D', application_cursor, modifier_param),
        KeyCode::Home => encode_csi_tilde_or_final(b'H', None, modifier_param),
        KeyCode::End => encode_csi_tilde_or_final(b'F', None, modifier_param),
        KeyCode::Insert => encode_csi_tilde_or_final(b'~', Some(2), modifier_param),
        KeyCode::Delete => encode_csi_tilde_or_final(b'~', Some(3), modifier_param),
        KeyCode::PageUp => encode_csi_tilde_or_final(b'~', Some(5), modifier_param),
        KeyCode::PageDown => encode_csi_tilde_or_final(b'~', Some(6), modifier_param),
        KeyCode::F(number) => encode_function_key(number, modifier_param),
        _ => Vec::new(),
    }
}

fn xterm_modifier_param(modifiers: crossterm::event::KeyModifiers) -> u8 {
    let mut value = 1u8;
    if modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
        value += 1;
    }
    if modifiers.contains(crossterm::event::KeyModifiers::ALT) {
        value += 2;
    }
    if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        value += 4;
    }
    value
}

fn encode_cursor_key(final_byte: u8, application_cursor: bool, modifier: u8) -> Vec<u8> {
    if modifier == 1 && application_cursor {
        vec![27, b'O', final_byte]
    } else if modifier == 1 {
        vec![27, b'[', final_byte]
    } else {
        format!("\x1b[1;{}{}", modifier, final_byte as char).into_bytes()
    }
}

fn encode_csi_tilde_or_final(final_byte: u8, number: Option<u8>, modifier: u8) -> Vec<u8> {
    match (number, modifier) {
        (None, 1) => vec![27, b'[', final_byte],
        (Some(number), 1) => format!("\x1b[{}~", number).into_bytes(),
        (Some(number), modifier) => format!("\x1b[{};{}~", number, modifier).into_bytes(),
        (None, modifier) => format!("\x1b[1;{}{}", modifier, final_byte as char).into_bytes(),
    }
}

fn encode_function_key(number: u8, modifier: u8) -> Vec<u8> {
    let (base, final_byte) = match number {
        1 => (None, b'P'),
        2 => (None, b'Q'),
        3 => (None, b'R'),
        4 => (None, b'S'),
        5 => (Some(15), b'~'),
        6 => (Some(17), b'~'),
        7 => (Some(18), b'~'),
        8 => (Some(19), b'~'),
        9 => (Some(20), b'~'),
        10 => (Some(21), b'~'),
        11 => (Some(23), b'~'),
        12 => (Some(24), b'~'),
        _ => return Vec::new(),
    };
    match (base, modifier) {
        (None, 1) => vec![27, b'O', final_byte],
        (None, modifier) => format!("\x1b[1;{}{}", modifier, final_byte as char).into_bytes(),
        (Some(number), 1) => format!("\x1b[{}~", number).into_bytes(),
        (Some(number), modifier) => format!("\x1b[{};{}~", number, modifier).into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_encoding_obeys_screen_mode_and_encoding() {
        assert!(encode_mouse_for_protocol(
            mouse(MouseEventKind::Up(MouseButton::Left)),
            crate::tui::term::screen::MouseProtocolMode::Press,
            crate::tui::term::screen::MouseProtocolEncoding::Sgr,
        )
        .is_none());
        assert_eq!(
            encode_mouse_for_protocol(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                crate::tui::term::screen::MouseProtocolMode::PressRelease,
                crate::tui::term::screen::MouseProtocolEncoding::Sgr,
            )
            .unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse_for_protocol(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                crate::tui::term::screen::MouseProtocolMode::Press,
                crate::tui::term::screen::MouseProtocolEncoding::Default,
            )
            .unwrap(),
            vec![27, b'[', b'M', 32, 37, 35]
        );
    }

    #[test]
    fn key_encoding_respects_application_cursor_and_backtab() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key_for_mode(up, false), b"\x1b[A");
        assert_eq!(encode_key_for_mode(up, true), b"\x1bOA");
        assert_eq!(
            encode_key_for_mode(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), false,),
            b"\x1b[Z"
        );
        assert!(
            !encode_key_for_mode(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), false)
                .is_empty()
        );
    }
}
