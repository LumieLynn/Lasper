pub mod manager;
mod mode;
mod osc52;
mod shell_prompt;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::term::CursorStyle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
pub use manager::{TerminalInputStatus, TerminalKeyOutcome, TerminalManager, TextSelection};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};
pub struct TerminalPanel;

impl TerminalPanel {
    pub(crate) fn register_footer_hints(
        &self,
        footer: &mut FooterRegistry,
        manager: &TerminalManager,
    ) {
        if manager.active_session().is_none() {
            return;
        }
        if manager.in_insert_mode() {
            footer.register(
                FooterHint::new("Alt+x", "exit insert mode")
                    .primary()
                    .on_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            );
            footer.register(FooterHint::new("Alt+1..9", "switch tabs"));
        } else if manager
            .active_session()
            .is_some_and(|session| session.mode().is_visual())
        {
            footer.register(FooterHint::new("Esc", "normal mode").primary());
            footer.register(FooterHint::new("h/j/k/l / arrows", "move"));
            footer.register(FooterHint::new("y", "yank").secondary());
        } else {
            footer.register(
                FooterHint::new("i/⏎/Alt+x", "insert mode")
                    .primary()
                    .on_key(KeyCode::Char('i')),
            );
            footer.register(FooterHint::new("Alt+1..9 / [/]", "switch tabs"));
            footer.register(FooterHint::new("v/V/Ctrl-v", "visual selection").secondary());
            footer.register(FooterHint::new("x", "close tab").on_key(KeyCode::Char('x')));
            footer.register(
                FooterHint::new("y", "yank")
                    .secondary()
                    .on_key(KeyCode::Char('y')),
            );
            crate::tui::foundation::scrolling::register_scroll_hints(footer);
        }
    }

    pub(crate) fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        manager: &mut TerminalManager,
        is_focused: bool,
        resize_mode: bool,
    ) -> Option<CursorStyle> {
        if manager.sessions.is_empty() {
            manager.tab_hitboxes.clear();
            manager.term_area = Rect::default();
            return None;
        }
        let active_idx = manager.active_idx;

        let tab_labels = manager
            .sessions
            .iter()
            .enumerate()
            .map(|(index, _)| manager.tab_label(index))
            .collect::<Vec<_>>();
        // Collect tab labels and session metadata before any mutable borrow.
        let t = crate::tui::foundation::theme::theme();
        let tab_specs = tab_labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| {
                let style = if index == active_idx {
                    Style::default()
                        .fg(if is_focused {
                            t.tab_active_focused
                        } else {
                            t.tab_active_unfocused
                        })
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.tab_inactive)
                };
                (index, format!(" {label} "), style)
            })
            .collect::<Vec<_>>();
        let tab_layout = manager.tab_viewport.layout(
            area,
            &tab_specs,
            "-",
            active_idx,
            Style::default().fg(t.tab_inactive),
        );
        manager.tab_hitboxes = tab_layout.hitboxes;
        let tabs_line = tab_layout.line;

        let session = &mut manager.sessions[active_idx];
        // Clone the Arc before locking so the guard does not hold an
        // immutable borrow of the whole session while resize state changes.
        let terminal = session.terminal.clone();
        let mut term = terminal.lock();

        let border_color = if resize_mode {
            crate::tui::foundation::theme::panel_border_color(true, is_focused, false)
        } else if is_focused {
            if session.is_insert_mode() {
                t.terminal_insert_border
            } else {
                t.border_focused
            }
        } else {
            t.border_panel_secondary
        };

        let title_suffix = match session.handle.lifecycle() {
            crate::domain::session::SessionLifecycle::Running if session.mode().is_insert() => {
                " [INSERT] ".to_string()
            }
            crate::domain::session::SessionLifecycle::Running if session.mode().is_visual() => {
                " [VISUAL] ".to_string()
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
        let viewport_offset = if session.mode().is_browser() {
            session.scroll_offset.min(max_scroll)
        } else {
            0
        };
        term.set_scrollback(viewport_offset);

        // Scoped block so the &Screen borrow is released before we restore
        // scrollback to 0 via &mut term.
        let (cursor_pos, cursor_style) = {
            let screen = term.screen();
            let size = screen.size();

            // Native rio selection is authoritative for visual mode. The old
            // stream selection remains the mouse fallback for compatibility.
            let sel = &session.selection;
            let sel_visible = sel.active || sel.anchor != sel.extent || session.yanked;
            let scroll_off = session.scroll_offset as i32;
            let native_selection = screen.browser_selection_range();

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

                        let mut style = self.get_cell_style(&cell);
                        let selected = native_selection
                            .map(|range| range.contains(row, col))
                            .unwrap_or(col >= sel_start && col <= sel_end);
                        if selected {
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

            let browser_cursor = is_focused
                && !resize_mode
                && session.handle.lifecycle().is_running()
                && session.mode().is_browser()
                && term_area.width > 0
                && term_area.height > 0;
            let cursor_active = is_focused
                && !resize_mode
                && session.is_insert_mode()
                && session.scroll_offset == 0
                && term_area.width > 0
                && term_area.height > 0;
            let style = if browser_cursor {
                Some(crate::tui::foundation::term::CursorStyle::SteadyBlock)
            } else if cursor_active {
                screen.cursor_style()
            } else {
                None
            };
            // Applications hide the cursor temporarily while redrawing. Keep
            // their style during that interval instead of resetting it.
            let position = (browser_cursor || (cursor_active && !screen.hide_cursor()))
                .then(|| screen.cursor_position());
            (position, style)
        }; // &Screen borrow released here

        // Insert mode keeps the live PTY viewport. Browser mode owns its
        // scrollback position and leaves rio there for the next motion.
        if !session.mode().is_browser() {
            term.set_scrollback(0);
        }

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
        cursor_style
    }

    fn get_cell_style(&self, cell: &crate::tui::foundation::term::Cell) -> Style {
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
        if attrs.dim() {
            style = style.add_modifier(Modifier::DIM);
        }
        if attrs.hidden() {
            style = style.add_modifier(Modifier::HIDDEN);
        }
        if attrs.strikeout() {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if attrs.slow_blink() {
            style = style.add_modifier(Modifier::SLOW_BLINK);
        }
        if attrs.rapid_blink() {
            style = style.add_modifier(Modifier::RAPID_BLINK);
        }

        style
    }

    fn map_color(&self, color: crate::tui::foundation::term::Color) -> Color {
        match color {
            crate::tui::foundation::term::Color::Default => Color::Reset,
            crate::tui::foundation::term::Color::Idx(i) => Color::Indexed(i),
            crate::tui::foundation::term::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

/// Encode a mouse event using the protocol selected by the terminal.
/// Coordinates are terminal-relative (0-based, top-left of the content area).
#[allow(dead_code)]
pub fn encode_mouse(mouse: crossterm::event::MouseEvent) -> Option<Vec<u8>> {
    encode_mouse_for_protocol(
        mouse,
        crate::tui::foundation::term::MouseProtocolMode::AnyMotion,
        crate::tui::foundation::term::MouseProtocolEncoding::Sgr,
    )
}

pub fn encode_mouse_for_protocol(
    mouse: crossterm::event::MouseEvent,
    mode: crate::tui::foundation::term::MouseProtocolMode,
    encoding: crate::tui::foundation::term::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind};

    if mode == crate::tui::foundation::term::MouseProtocolMode::None
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
        crate::tui::foundation::term::MouseProtocolEncoding::Sgr => {
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
        crate::tui::foundation::term::MouseProtocolEncoding::Default
        | crate::tui::foundation::term::MouseProtocolEncoding::Utf8 => {
            // Legacy encodings use 1-based coordinates offset by 32.  The
            // default byte form is limited to 223 cells; UTF-8 extends it.
            let col = mouse.column.saturating_add(33);
            let row = mouse.row.saturating_add(33);
            if encoding == crate::tui::foundation::term::MouseProtocolEncoding::Default
                && (col > 255 || row > 255)
            {
                return None;
            }

            let mut sequence = vec![b'\x1b', b'[', b'M'];
            if encoding == crate::tui::foundation::term::MouseProtocolEncoding::Default {
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
    mode: crate::tui::foundation::term::MouseProtocolMode,
) -> bool {
    use crossterm::event::MouseEventKind;
    match mode {
        crate::tui::foundation::term::MouseProtocolMode::None => false,
        crate::tui::foundation::term::MouseProtocolMode::Press => {
            matches!(
                kind,
                MouseEventKind::Down(_)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
        }
        crate::tui::foundation::term::MouseProtocolMode::PressRelease => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        crate::tui::foundation::term::MouseProtocolMode::ButtonMotion => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        crate::tui::foundation::term::MouseProtocolMode::AnyMotion => true,
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
    encode_key_for_terminal(
        key,
        application_cursor,
        crate::tui::foundation::term::TerminalKeyProtocol::Legacy,
    )
}

pub fn encode_key_for_terminal(
    key: crossterm::event::KeyEvent,
    application_cursor: bool,
    key_protocol: crate::tui::foundation::term::TerminalKeyProtocol,
) -> Vec<u8> {
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
        KeyCode::Enter => encode_enter_key(modifiers, key_protocol),
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

fn encode_enter_key(
    modifiers: crossterm::event::KeyModifiers,
    key_protocol: crate::tui::foundation::term::TerminalKeyProtocol,
) -> Vec<u8> {
    use crossterm::event::KeyModifiers;

    let modified = modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
    if modified {
        let modifier = xterm_modifier_param(modifiers);
        return match key_protocol {
            crate::tui::foundation::term::TerminalKeyProtocol::Kitty => {
                format!("\x1b[13;{modifier}u").into_bytes()
            }
            crate::tui::foundation::term::TerminalKeyProtocol::ModifyOtherKeys(_) => {
                format!("\x1b[27;{modifier};13~").into_bytes()
            }
            crate::tui::foundation::term::TerminalKeyProtocol::Legacy => {
                if modifiers.contains(KeyModifiers::ALT) {
                    vec![27, b'\r']
                } else {
                    vec![b'\r']
                }
            }
        };
    }

    if modifiers.contains(KeyModifiers::ALT) {
        vec![27, b'\r']
    } else {
        vec![b'\r']
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
    use crate::tui::foundation::term::TerminalCore;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::style::Color;

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
            crate::tui::foundation::term::MouseProtocolMode::Press,
            crate::tui::foundation::term::MouseProtocolEncoding::Sgr,
        )
        .is_none());
        assert_eq!(
            encode_mouse_for_protocol(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                crate::tui::foundation::term::MouseProtocolMode::PressRelease,
                crate::tui::foundation::term::MouseProtocolEncoding::Sgr,
            )
            .unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse_for_protocol(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                crate::tui::foundation::term::MouseProtocolMode::Press,
                crate::tui::foundation::term::MouseProtocolEncoding::Default,
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

    #[test]
    fn modified_enter_uses_only_the_protocol_requested_by_the_application() {
        use crate::tui::foundation::term::TerminalKeyProtocol;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key_for_terminal(shift_enter, false, TerminalKeyProtocol::Legacy),
            b"\r"
        );
        assert_eq!(
            encode_key_for_terminal(shift_enter, false, TerminalKeyProtocol::ModifyOtherKeys(2)),
            b"\x1b[27;2;13~"
        );
        assert_eq!(
            encode_key_for_terminal(shift_enter, false, TerminalKeyProtocol::Kitty),
            b"\x1b[13;2u"
        );
    }

    #[test]
    fn terminal_panel_preserves_rgb_cell_styles() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(
            b"\x1b[38;2;12;34;56mforeground\x1b[48;2;78;90;123mbackground",
            &mut Vec::new(),
        );
        let panel = TerminalPanel;

        let foreground = terminal.screen().cell(0, 0).expect("foreground cell");
        let foreground_style = panel.get_cell_style(&foreground);
        assert_eq!(foreground_style.fg, Some(Color::Rgb(12, 34, 56)));

        let background = terminal.screen().cell(0, 10).expect("background cell");
        let background_style = panel.get_cell_style(&background);
        assert_eq!(background_style.bg, Some(Color::Rgb(78, 90, 123)));
    }

    #[test]
    fn terminal_panel_preserves_rgb_background_on_blank_cells() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[48;2;78;90;123m ", &mut Vec::new());
        let panel = TerminalPanel;

        let cell = terminal.screen().cell(0, 0).expect("blank colored cell");
        assert_eq!(
            panel.get_cell_style(&cell).bg,
            Some(Color::Rgb(78, 90, 123))
        );
    }

    #[test]
    fn terminal_panel_preserves_indexed_background_on_blank_cells() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[48;5;8m ", &mut Vec::new());
        let panel = TerminalPanel;

        let cell = terminal.screen().cell(0, 0).expect("blank colored cell");
        assert_eq!(panel.get_cell_style(&cell).bg, Some(Color::Indexed(8)));
    }

    #[test]
    fn terminal_panel_preserves_rgb_background_after_line_erase() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[48;2;78;90;123m\x1b[2K", &mut Vec::new());
        let panel = TerminalPanel;

        let cell = terminal.screen().cell(0, 0).expect("erased colored cell");
        assert_eq!(
            panel.get_cell_style(&cell).bg,
            Some(Color::Rgb(78, 90, 123))
        );
    }

    #[test]
    fn terminal_panel_preserves_inverse_and_extended_text_attributes() {
        let panel = TerminalPanel;
        for (sequence, modifier) in [
            (b"\x1b[1mstyled".as_slice(), Modifier::BOLD),
            (b"\x1b[2mstyled".as_slice(), Modifier::DIM),
            (b"\x1b[5mstyled".as_slice(), Modifier::SLOW_BLINK),
            (b"\x1b[7mstyled".as_slice(), Modifier::REVERSED),
            (b"\x1b[8mstyled".as_slice(), Modifier::HIDDEN),
            (b"\x1b[9mstyled".as_slice(), Modifier::CROSSED_OUT),
        ] {
            let mut terminal = TerminalCore::new(24, 80, 100);
            terminal.process(sequence, &mut Vec::new());
            let cell = terminal.screen().cell(0, 0).unwrap();
            let style = panel.get_cell_style(&cell);
            assert!(
                style.has_modifier(modifier),
                "missing modifier {modifier:?}"
            );
        }
    }
}
