//! Rio VT core adapter used by the terminal panel.
//!
//! The panel owns PTYs and rendering, while Rio owns terminal semantics.  The
//! adapter deliberately exposes Lasper's small terminal-facing vocabulary so
//! the rest of the TUI does not depend on Rio types.

use compact_str::CompactString;
use rio_vt::ansi::CursorShape;
use rio_vt::ansi::KeyboardModes;
use rio_vt::config::colors::{AnsiColor, ColorRgb, NamedColor};
use rio_vt::crosswords::grid::Scroll;
use rio_vt::crosswords::pos::{Boundary, Column, Line, Pos, Side};
use rio_vt::crosswords::style::{Style, StyleFlags};
use rio_vt::crosswords::vi_mode::ViMotion;
use rio_vt::crosswords::{Crosswords, CrosswordsSize, Mode};
use rio_vt::event::{EventListener, RioEvent, WindowId};
use rio_vt::performer::handler::{Handler, Processor};
use rio_vt::selection::{Selection, SelectionType};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use super::appearance::default_query_colors;
use super::attrs::Attrs;
use super::color::Color;
use super::cursor::CursorStyle;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub height: u16,
    pub width: u16,
}

#[derive(Clone, Debug)]
pub enum VtEvent {
    Bell,
    Reply(CompactString),
}

enum ListenerEvent {
    Bell,
    Reply(CompactString),
    ColorRequest(
        usize,
        Arc<dyn Fn(ColorRgb) -> String + Sync + Send + 'static>,
    ),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

/// Encoding requested by the application running inside the PTY.
///
/// Modified keys must only use an extended sequence after the application has
/// enabled the corresponding protocol. Sending CSI-u to a traditional shell
/// that did not request it would turn a key press into an unknown escape
/// sequence instead of preserving ordinary terminal behavior.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum TerminalKeyProtocol {
    #[default]
    Legacy,
    ModifyOtherKeys(u8),
    Kitty,
}

/// Motion used by the terminal panel's local browser mode.
///
/// This is intentionally a Lasper-owned vocabulary.  The VT adapter is the
/// only place that translates it to rio's vi implementation, so panel input
/// code does not depend on rio's public types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserMotion {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    WordLeft,
    WordRight,
}

/// Selection shape exposed by local browser mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserSelectionKind {
    Character,
    Line,
    Block,
}

/// A visible selection range in panel coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserSelectionRange {
    pub start_row: i32,
    pub start_col: u16,
    pub end_row: i32,
    pub end_col: u16,
    pub is_block: bool,
}

impl BrowserSelectionRange {
    #[must_use]
    pub fn contains(self, row: u16, col: u16) -> bool {
        let row = i32::from(row);
        let col = i32::from(col);
        if row < self.start_row || row > self.end_row {
            return false;
        }
        if self.is_block {
            return col >= i32::from(self.start_col) && col <= i32::from(self.end_col);
        }
        if self.start_row == self.end_row {
            return col >= i32::from(self.start_col) && col <= i32::from(self.end_col);
        }
        if row == self.start_row {
            return col >= i32::from(self.start_col);
        }
        if row == self.end_row {
            return col <= i32::from(self.end_col);
        }
        true
    }
}

#[derive(Default)]
struct ListenerState {
    events: Vec<ListenerEvent>,
    cursor_style_requested: bool,
}

#[derive(Clone, Default)]
struct Listener {
    state: Arc<Mutex<ListenerState>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _id: WindowId) {
        if matches!(event, RioEvent::CursorBlinkingChange) {
            self.state
                .lock()
                .expect("rio event queue lock poisoned")
                .cursor_style_requested = true;
            return;
        }
        // Rio's graphical frontend notifications have no consumer here. In
        // particular, viewport updates can emit events without any PTY output
        // arriving to drain this queue.
        let event = match event {
            RioEvent::PtyWrite(_, text) => ListenerEvent::Reply(text.into()),
            RioEvent::Bell(_) => ListenerEvent::Bell,
            RioEvent::ColorRequest(_, index, formatter) => {
                ListenerEvent::ColorRequest(index, formatter)
            }
            _ => return,
        };
        self.state
            .lock()
            .expect("rio event queue lock poisoned")
            .events
            .push(event);
    }
}

/// A terminal parser and screen owned by one PTY session.
pub struct TerminalCore {
    core: Crosswords<Listener>,
    processor: Processor,
    listener: Listener,
    input_buffer: Vec<u8>,
    query_colors: [ColorRgb; 3],
    suppress_initial_line_breaks: bool,
    clear_primary_history_on_alt_exit: bool,
    cursor_style_requested: bool,
    saved_primary_cursor: Option<EmbeddedCursorState>,
    browser_cursor_enabled: bool,
    saved_browser_cursor: Option<Pos>,
}

#[derive(Clone, Copy)]
struct EmbeddedCursorState {
    shape: CursorShape,
    blinking: bool,
    requested: bool,
}

impl TerminalCore {
    #[cfg(test)]
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self::new_with_host_appearance(rows, cols, scrollback_len, default_query_colors(), None)
    }

    pub(crate) fn new_with_host_appearance(
        rows: u16,
        cols: u16,
        scrollback_len: usize,
        query_colors: [ColorRgb; 3],
        cursor_style: Option<CursorStyle>,
    ) -> Self {
        let listener = Listener::default();
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut terminal = Self {
            core: Crosswords::new(
                CrosswordsSize::new(cols as usize, rows as usize),
                CursorShape::Block,
                listener.clone(),
                WindowId::from(0),
                0,
                scrollback_len,
            ),
            processor: Processor::default(),
            listener,
            input_buffer: Vec::new(),
            query_colors,
            suppress_initial_line_breaks: false,
            clear_primary_history_on_alt_exit: false,
            cursor_style_requested: false,
            saved_primary_cursor: None,
            browser_cursor_enabled: false,
            saved_browser_cursor: None,
        };
        if let Some(style) = cursor_style {
            terminal.set_host_cursor_style(style);
        }
        terminal
    }

    pub(crate) fn set_query_colors(&mut self, query_colors: [ColorRgb; 3]) {
        self.query_colors = query_colors;
    }

    pub(crate) fn set_host_cursor_style(&mut self, style: CursorStyle) {
        let (shape, blinking) = match style {
            CursorStyle::BlinkingBlock => (CursorShape::Block, true),
            CursorStyle::SteadyBlock => (CursorShape::Block, false),
            CursorStyle::BlinkingUnderline => (CursorShape::Underline, true),
            CursorStyle::SteadyUnderline => (CursorShape::Underline, false),
            CursorStyle::BlinkingBar => (CursorShape::Beam, true),
            CursorStyle::SteadyBar => (CursorShape::Beam, false),
        };
        self.core.default_cursor_shape = shape;
        if !self.cursor_style_requested {
            self.core.cursor_shape = shape;
            self.core.blinking_cursor = blinking;
        }
    }

    /// Feed PTY output into the VT parser and return replies requested by the
    /// application (for example DSR cursor reports).
    pub fn process(&mut self, data: &[u8], events: &mut Vec<VtEvent>) {
        self.input_buffer.extend_from_slice(data);
        self.process_input_buffer();
        let mut queued = self
            .listener
            .state
            .lock()
            .expect("rio event queue lock poisoned");
        for event in queued.events.drain(..) {
            match event {
                ListenerEvent::Bell => events.push(VtEvent::Bell),
                ListenerEvent::Reply(reply) => events.push(VtEvent::Reply(reply)),
                ListenerEvent::ColorRequest(index, formatter) => {
                    events.push(VtEvent::Reply(formatter(self.query_color(index)).into()));
                }
            }
        }
    }

    fn query_color(&self, index: usize) -> ColorRgb {
        let fallback = match index {
            value if value == NamedColor::Foreground as usize => self.query_colors[0],
            value if value == NamedColor::Background as usize => self.query_colors[1],
            value if value == NamedColor::Cursor as usize => self.query_colors[2],
            _ => self.query_colors[1],
        };

        if index < rio_vt::config::colors::term::COUNT {
            self.core.colors()[index]
                .map(ColorRgb::from_color_arr)
                .unwrap_or(fallback)
        } else {
            fallback
        }
    }

    fn process_input_buffer(&mut self) {
        const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";

        loop {
            if let Some(index) = self
                .input_buffer
                .windows(ALT_SCREEN_ENTER.len())
                .position(|window| window == ALT_SCREEN_ENTER)
            {
                let split = index + ALT_SCREEN_ENTER.len();
                let prefix: Vec<u8> = self.input_buffer.drain(..split).collect();
                let was_alt_screen = self.core.mode().contains(Mode::ALT_SCREEN);
                self.advance(&prefix);
                if !was_alt_screen && self.core.mode().contains(Mode::ALT_SCREEN) {
                    self.saved_primary_cursor = Some(self.cursor_state());
                    // Rio carries the active cursor into the alternate grid;
                    // DECSET 1049 presents a fresh viewport at home.
                    Handler::goto(&mut self.core, Line(0), Column(0));
                }
                continue;
            }

            let keep = longest_suffix_prefix(&self.input_buffer, ALT_SCREEN_ENTER);
            let process_len = self.input_buffer.len().saturating_sub(keep);
            if process_len == 0 {
                break;
            }
            let bytes: Vec<u8> = self.input_buffer.drain(..process_len).collect();
            self.advance(&bytes);
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        let was_alt_screen = self.core.mode().contains(Mode::ALT_SCREEN);
        if self.suppress_initial_line_breaks {
            for byte in bytes {
                if *byte == b'\n' && self.core.cursor().pos == Pos::new(Line(0), Column(0)) {
                    continue;
                }
                self.processor
                    .advance(&mut self.core, std::slice::from_ref(byte));
                if self.core.cursor().pos != Pos::new(Line(0), Column(0)) {
                    self.suppress_initial_line_breaks = false;
                }
            }
        } else {
            self.processor.advance(&mut self.core, bytes);
        }
        self.consume_cursor_style_request();
        let is_alt_screen = self.core.mode().contains(Mode::ALT_SCREEN);
        if was_alt_screen && !is_alt_screen {
            self.restore_primary_cursor();
        }
        if self.clear_primary_history_on_alt_exit && was_alt_screen && !is_alt_screen {
            self.core.clear_saved_history();
            self.core.grid.update_history(0);
            self.clear_primary_history_on_alt_exit = false;
        }
    }

    fn cursor_state(&self) -> EmbeddedCursorState {
        EmbeddedCursorState {
            shape: self.core.cursor_shape,
            blinking: self.core.blinking_cursor,
            requested: self.cursor_style_requested,
        }
    }

    fn consume_cursor_style_request(&mut self) {
        let mut state = self
            .listener
            .state
            .lock()
            .expect("rio event queue lock poisoned");
        if state.cursor_style_requested {
            self.cursor_style_requested = true;
            state.cursor_style_requested = false;
        }
    }

    fn restore_primary_cursor(&mut self) {
        let Some(saved) = self.saved_primary_cursor.take() else {
            return;
        };
        self.core.cursor_shape = saved.shape;
        self.core.blinking_cursor = saved.blinking;
        self.cursor_style_requested = saved.requested;
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.core.resize(CrosswordsSize::new(
            cols.max(1) as usize,
            rows.max(1) as usize,
        ));
    }

    pub fn set_scrollback(&mut self, rows: usize) {
        let target = rows.min(self.core.history_size());
        let current = self.core.display_offset();
        if target == current {
            return;
        }
        self.core
            .scroll_display(Scroll::Delta(target as i32 - current as i32));
    }

    /// Show or hide the local browser cursor used by the terminal panel.
    ///
    /// This toggles rio's internal VI cursor only.  It does not send anything
    /// to the PTY, so an application running inside the terminal remains in
    /// its own input mode and terminal protocol state.
    pub(crate) fn set_browser_mode(&mut self, active: bool) {
        let enabled = self.core.mode().contains(Mode::VI);
        if active {
            if !enabled {
                self.toggle_internal_vi_mode();
            }
            if let Some(pos) = self.saved_browser_cursor.take() {
                let pos = pos.grid_clamp(&self.core.grid, Boundary::Grid);
                self.core.vi_goto_pos(pos);
            }
        } else {
            if enabled {
                self.saved_browser_cursor = Some(self.core.vi_mode_cursor.pos);
                self.toggle_internal_vi_mode();
            }
            self.core.selection = None;
        }
        self.browser_cursor_enabled = active;
    }

    /// Rio reports its internal VI-mode toggle through the same event used for
    /// terminal-program cursor changes. The toggle is a local browser detail,
    /// so it must not turn on `cursor_style_requested` on the next PTY write.
    fn toggle_internal_vi_mode(&mut self) {
        self.consume_cursor_style_request();
        self.core.toggle_vi_mode();
        self.listener
            .state
            .lock()
            .expect("rio event queue lock poisoned")
            .cursor_style_requested = false;
    }

    #[must_use]
    pub(crate) fn browser_mode(&self) -> bool {
        self.browser_cursor_enabled
    }

    pub(crate) fn browser_motion(&mut self, motion: BrowserMotion) {
        let motion = match motion {
            BrowserMotion::Up => ViMotion::Up,
            BrowserMotion::Down => ViMotion::Down,
            BrowserMotion::Left => ViMotion::Left,
            BrowserMotion::Right => ViMotion::Right,
            BrowserMotion::First => ViMotion::First,
            BrowserMotion::Last => ViMotion::Last,
            BrowserMotion::WordLeft => ViMotion::WordLeft,
            BrowserMotion::WordRight => ViMotion::WordRight,
        };
        self.core.vi_motion(motion);
        let position = self.core.vi_mode_cursor.pos;
        self.core.scroll_to_pos(position);
    }

    pub(crate) fn browser_goto(&mut self, row: u16, col: u16) {
        let size = self.screen().size();
        if row >= size.height || col >= size.width || !self.browser_mode() {
            return;
        }
        self.core.vi_goto_pos(Pos::new(
            Line::from(row as usize) - self.core.display_offset(),
            Column(col as usize),
        ));
    }

    #[must_use]
    pub(crate) fn scrollback_offset(&self) -> usize {
        self.core.display_offset()
    }

    pub(crate) fn begin_browser_selection(&mut self, kind: BrowserSelectionKind) {
        if !self.browser_mode() {
            return;
        }
        let ty = match kind {
            BrowserSelectionKind::Character => SelectionType::Simple,
            BrowserSelectionKind::Line => SelectionType::Lines,
            BrowserSelectionKind::Block => SelectionType::Block,
        };
        let cursor = self.core.vi_mode_cursor.pos;
        let mut selection = Selection::new(ty, cursor, Side::Left);
        // Seed the range at the cursor. Rio treats an unseeded simple or
        // block selection as empty until its end side is established.
        selection.update(cursor, Side::Right);
        selection.include_all();
        self.core.selection = Some(selection);
    }

    pub(crate) fn clear_browser_selection(&mut self) {
        self.core.selection = None;
    }

    #[must_use]
    pub(crate) fn browser_selection_text(&self) -> Option<String> {
        self.core.selection_to_string()
    }

    pub fn clear_scrollback(&mut self) {
        if self.core.mode().contains(Mode::ALT_SCREEN) {
            self.clear_primary_history_on_alt_exit = true;
        } else {
            self.core.clear_saved_history();
            self.core.grid.update_history(0);
        }
        self.core.scroll_display(Scroll::Bottom);
    }

    pub fn suppress_initial_line_breaks(&mut self) {
        self.suppress_initial_line_breaks = true;
    }

    #[must_use]
    pub fn screen(&self) -> TerminalScreen<'_> {
        TerminalScreen {
            core: &self.core,
            cursor_style_requested: self.cursor_style_requested,
            browser_cursor_enabled: self.browser_cursor_enabled,
        }
    }
}

fn longest_suffix_prefix(bytes: &[u8], pattern: &[u8]) -> usize {
    (1..pattern.len().min(bytes.len()) + 1)
        .rev()
        .find(|length| bytes.ends_with(&pattern[..*length]))
        .unwrap_or(0)
}

pub struct TerminalScreen<'a> {
    core: &'a Crosswords<Listener>,
    cursor_style_requested: bool,
    browser_cursor_enabled: bool,
}

impl TerminalScreen<'_> {
    #[must_use]
    pub fn size(&self) -> Size {
        Size {
            height: self.core.screen_lines() as u16,
            width: self.core.columns() as u16,
        }
    }

    #[must_use]
    pub fn row0_count(&self) -> usize {
        self.core.history_size()
    }

    #[must_use]
    pub fn cursor_position(&self) -> (u16, u16) {
        let position = if self.browser_cursor_enabled {
            self.core.cursor().pos
        } else {
            self.core.grid.cursor.pos
        };
        (position.row.0.max(0) as u16, position.col.0 as u16)
    }

    #[must_use]
    pub(crate) fn browser_selection_range(&self) -> Option<BrowserSelectionRange> {
        let selection = self.core.selection.as_ref()?;
        let range = selection.to_range(self.core)?;
        let offset = self.core.display_offset() as i32;
        Some(BrowserSelectionRange {
            start_row: range.start.row.0 + offset,
            start_col: range.start.col.0 as u16,
            end_row: range.end.row.0 + offset,
            end_col: range.end.col.0 as u16,
            is_block: range.is_block,
        })
    }

    #[must_use]
    pub fn hide_cursor(&self) -> bool {
        self.core.cursor().content == CursorShape::Hidden
    }

    #[must_use]
    pub(crate) fn cursor_style(&self) -> Option<CursorStyle> {
        // Rio's configured default is screen state, not an application request
        // to overwrite the real host cursor.
        if !self.cursor_style_requested {
            return None;
        }
        match (self.core.cursor_shape, self.core.blinking_cursor) {
            (CursorShape::Hidden, _) => None,
            (CursorShape::Block, true) => Some(CursorStyle::BlinkingBlock),
            (CursorShape::Block, false) => Some(CursorStyle::SteadyBlock),
            (CursorShape::Underline, true) => Some(CursorStyle::BlinkingUnderline),
            (CursorShape::Underline, false) => Some(CursorStyle::SteadyUnderline),
            (CursorShape::Beam, true) => Some(CursorStyle::BlinkingBar),
            (CursorShape::Beam, false) => Some(CursorStyle::SteadyBar),
        }
    }

    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.core.mode().contains(Mode::APP_CURSOR)
    }

    #[must_use]
    pub fn key_protocol(&self) -> TerminalKeyProtocol {
        if self
            .core
            .keyboard_mode()
            .contains(KeyboardModes::DISAMBIGUATE_ESC_CODES)
        {
            TerminalKeyProtocol::Kitty
        } else if let Some(level) = self.core.modify_other_keys() {
            TerminalKeyProtocol::ModifyOtherKeys(level)
        } else {
            TerminalKeyProtocol::Legacy
        }
    }

    #[must_use]
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        let mode = self.core.mode();
        if mode.contains(Mode::MOUSE_REPORT_X10) {
            MouseProtocolMode::Press
        } else if mode.contains(Mode::MOUSE_MOTION) {
            MouseProtocolMode::AnyMotion
        } else if mode.contains(Mode::MOUSE_DRAG) {
            MouseProtocolMode::ButtonMotion
        } else if mode.contains(Mode::MOUSE_REPORT_CLICK) {
            MouseProtocolMode::PressRelease
        } else {
            MouseProtocolMode::None
        }
    }

    #[must_use]
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        let mode = self.core.mode();
        if mode.contains(Mode::SGR_MOUSE) {
            MouseProtocolEncoding::Sgr
        } else if mode.contains(Mode::UTF8_MOUSE) {
            MouseProtocolEncoding::Utf8
        } else {
            MouseProtocolEncoding::Default
        }
    }

    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<Cell> {
        let size = self.size();
        if row >= size.height || col >= size.width {
            return None;
        }
        let pos = Pos::new(
            Line::from(row as usize) - self.core.display_offset(),
            Column(col as usize),
        );
        let square = self.core.grid[pos];
        let text: CompactString = self
            .core
            .grid
            .cell_text(pos)
            .filter(|character| *character != '\0')
            .collect();
        let style = self.core.grid.style_of(&square);
        Some(Cell::new(text, style))
    }

    /// Extract a stream selection using the same row-coordinate contract as
    /// the previous terminal core: rows are relative to the top of the
    /// scrollback buffer, so negative rows refer to history.
    #[must_use]
    pub fn get_selected_text(
        &self,
        anchor_col: i32,
        anchor_row: i32,
        extent_col: i32,
        extent_row: i32,
    ) -> String {
        let first_row = anchor_row.min(extent_row);
        let last_row = anchor_row.max(extent_row);
        let downward =
            anchor_row < extent_row || (anchor_row == extent_row && anchor_col <= extent_col);
        let mut output = String::new();
        let history = self.core.history_size() as i32;
        let lines = self.core.screen_lines() as i32;
        let columns = self.core.columns() as i32;

        for row in first_row..=last_row {
            let (start, end) = if anchor_row == extent_row {
                (anchor_col.min(extent_col), anchor_col.max(extent_col))
            } else if row == first_row {
                if downward {
                    (anchor_col, columns.saturating_sub(1))
                } else {
                    (extent_col, columns.saturating_sub(1))
                }
            } else if row == last_row {
                if downward {
                    (0, extent_col)
                } else {
                    (0, anchor_col)
                }
            } else {
                (0, columns.saturating_sub(1))
            };

            if row < -history || row >= lines {
                continue;
            }
            let line = Line(row);
            let start = start.max(0) as u16;
            let end = end.min(columns.saturating_sub(1)).max(-1) as u16;
            if start <= end {
                let mut line_text = String::new();
                for col in start..=end {
                    let pos = Pos::new(line, Column(col as usize));
                    line_text.extend(
                        self.core
                            .grid
                            .cell_text(pos)
                            .filter(|character| *character != '\0'),
                    );
                }
                output.push_str(line_text.trim_end_matches(' '));
            }
            if row != last_row {
                output.push('\n');
            }
        }
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    text: CompactString,
    attrs: Attrs,
}

impl Cell {
    fn new(text: CompactString, style: Style) -> Self {
        let mut attrs = Attrs {
            fgcolor: map_color(style.fg),
            bgcolor: map_color(style.bg),
            ..Attrs::default()
        };
        attrs.set_bold(style.flags.contains(StyleFlags::BOLD));
        attrs.set_italic(style.flags.contains(StyleFlags::ITALIC));
        attrs.set_underline(style.flags.intersects(StyleFlags::ALL_UNDERLINES));
        attrs.set_inverse(style.flags.contains(StyleFlags::INVERSE));
        attrs.set_dim(style.flags.contains(StyleFlags::DIM));
        attrs.set_hidden(style.flags.contains(StyleFlags::HIDDEN));
        attrs.set_strikeout(style.flags.contains(StyleFlags::STRIKEOUT));
        attrs.set_slow_blink(style.flags.contains(StyleFlags::SLOW_BLINK));
        attrs.set_rapid_blink(style.flags.contains(StyleFlags::RAPID_BLINK));
        Self { text, attrs }
    }

    #[must_use]
    pub fn contents(&self) -> &str {
        self.text.as_str()
    }

    #[must_use]
    pub fn attrs(&self) -> &Attrs {
        &self.attrs
    }
}

fn map_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(index) => Color::Idx(index),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground | NamedColor::LightForeground | NamedColor::DimForeground => {
                Color::Default
            }
            NamedColor::Background => Color::Default,
            NamedColor::Black | NamedColor::DimBlack => Color::BLACK,
            NamedColor::Red | NamedColor::DimRed => Color::RED,
            NamedColor::Green | NamedColor::DimGreen => Color::GREEN,
            NamedColor::Yellow | NamedColor::DimYellow => Color::YELLOW,
            NamedColor::Blue | NamedColor::DimBlue => Color::BLUE,
            NamedColor::Magenta | NamedColor::DimMagenta => Color::MAGENTA,
            NamedColor::Cyan | NamedColor::DimCyan => Color::CYAN,
            NamedColor::White | NamedColor::DimWhite => Color::WHITE,
            NamedColor::LightBlack => Color::BRIGHT_BLACK,
            NamedColor::LightRed => Color::BRIGHT_RED,
            NamedColor::LightGreen => Color::BRIGHT_GREEN,
            NamedColor::LightYellow => Color::BRIGHT_YELLOW,
            NamedColor::LightBlue => Color::BRIGHT_BLUE,
            NamedColor::LightMagenta => Color::BRIGHT_MAGENTA,
            NamedColor::LightCyan => Color::BRIGHT_CYAN,
            NamedColor::LightWhite => Color::BRIGHT_WHITE,
            NamedColor::Cursor => Color::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserMotion, BrowserSelectionKind, CursorStyle, TerminalCore};

    #[test]
    fn initial_cursor_is_inherited_until_rio_receives_an_explicit_request() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"prompt\x1b[?25l", &mut Vec::new());
        assert_eq!(terminal.screen().cursor_style(), None);
        terminal.process(b"\x1b[?25h", &mut Vec::new());
        assert!(!terminal.screen().hide_cursor());
        assert_eq!(terminal.screen().cursor_style(), None);

        // Even a request matching Rio's initial state is an explicit override.
        terminal.process(b"\x1b[2 q", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBlock)
        );
    }

    #[test]
    fn blinking_only_requests_use_the_captured_host_cursor_shape() {
        let mut terminal = TerminalCore::new_with_host_appearance(
            24,
            80,
            100,
            super::default_query_colors(),
            Some(CursorStyle::BlinkingBar),
        );
        assert_eq!(terminal.screen().cursor_style(), None);
        terminal.process(b"\x1b[?12l", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBar)
        );
        terminal.process(b"\x1b[?12h", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::BlinkingBar)
        );
    }

    #[test]
    fn alternate_screen_restores_the_primary_cursor_style_after_nvim_exits() {
        let mut terminal = TerminalCore::new_with_host_appearance(
            24,
            80,
            100,
            super::default_query_colors(),
            Some(CursorStyle::SteadyUnderline),
        );
        assert_eq!(terminal.screen().cursor_style(), None);

        terminal.process(b"\x1b[?1049h\x1b[5 q", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::BlinkingBar)
        );

        // Neovim resets its cursor before leaving the alternate screen. The
        // reset belongs to the alternate screen and must not leak into the
        // shell's primary-screen state.
        terminal.process(b"\x1b[0 q\x1b[?1049l", &mut Vec::new());
        assert_eq!(terminal.screen().cursor_style(), None);

        terminal.process(b"\x1b[?1049h", &mut Vec::new());
        assert_eq!(terminal.screen().cursor_style(), None);
        terminal.process(b"\x1b[6 q", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBar)
        );
    }

    #[test]
    fn actual_nvim_exit_sequence_does_not_leave_its_normal_cursor_on_the_shell() {
        let mut terminal = TerminalCore::new_with_host_appearance(
            24,
            80,
            100,
            super::default_query_colors(),
            Some(CursorStyle::SteadyUnderline),
        );
        terminal.process(
            b"\x1b[?1049h\x1b[?25l\x1b[2 q\x1b[?25h\x1b[?25l\x1b[6 q",
            &mut Vec::new(),
        );
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBar)
        );
        terminal.process(
            b"\x1b[?25h\x1b[?25l\x1b[2 q\x1b[?1002l\x1b[?1006l\x1b(B\x1b[m\x1b[?25h\x1b[?1l\x1b>\x1b[>4;0m\x1b[?2004l\x1b[?1004l\x1b[c\x1b[?25h\x1b[?25l\x1b[?1049l",
            &mut Vec::new(),
        );
        assert_eq!(terminal.screen().cursor_style(), None);
    }

    #[test]
    fn nvim_cursor_restore_also_works_through_the_session_startup_filter() {
        let mut terminal = TerminalCore::new_with_host_appearance(
            24,
            80,
            100,
            super::default_query_colors(),
            Some(CursorStyle::SteadyUnderline),
        );
        terminal.suppress_initial_line_breaks();
        terminal.process(
            b"\x1b[?1049h\x1b[?25l\x1b[2 q\x1b[?25h\x1b[?25l\x1b[6 q",
            &mut Vec::new(),
        );
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBar)
        );
        terminal.process(
            b"\x1b[?25h\x1b[?25l\x1b[2 q\x1b[?1002l\x1b[?1006l\x1b(B\x1b[m\x1b[?25h\x1b[?1l\x1b>\x1b[>4;0m\x1b[?2004l\x1b[?1004l\x1b[c\x1b[?25h\x1b[?25l\x1b[?1049l",
            &mut Vec::new(),
        );
        assert_eq!(terminal.screen().cursor_style(), None);
    }

    #[test]
    fn rio_exposes_decscusr_styles_across_output_chunks() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        for (parameter, style) in [
            (1, CursorStyle::BlinkingBlock),
            (2, CursorStyle::SteadyBlock),
            (3, CursorStyle::BlinkingUnderline),
            (4, CursorStyle::SteadyUnderline),
            (5, CursorStyle::BlinkingBar),
            (6, CursorStyle::SteadyBar),
        ] {
            terminal.process(b"\x1b[", &mut Vec::new());
            terminal.process(format!("{parameter} q").as_bytes(), &mut Vec::new());
            assert_eq!(terminal.screen().cursor_style(), Some(style));
        }
    }

    #[test]
    fn rio_preserves_cursor_style_when_blinking_and_visibility_change() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[6 q\x1b[?12h", &mut Vec::new());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::BlinkingBar)
        );
        terminal.process(b"\x1b[?25l", &mut Vec::new());
        assert!(terminal.screen().hide_cursor());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::BlinkingBar)
        );
        terminal.process(b"\x1b[?25h\x1b[?12l", &mut Vec::new());
        assert!(!terminal.screen().hide_cursor());
        assert_eq!(
            terminal.screen().cursor_style(),
            Some(CursorStyle::SteadyBar)
        );
    }

    #[test]
    fn rio_preserves_alternate_screen_contents() {
        let mut parser = TerminalCore::new(24, 80, 100);
        parser.process(b"history\r\n", &mut Vec::new());
        parser.process(b"\x1b[?1049h\x1b[Halternate", &mut Vec::new());
        let cell = parser.screen().cell(0, 0).expect("cell");
        assert_eq!(cell.contents(), "a");
    }

    #[test]
    fn rio_handles_alternate_screen_sequence_split_across_output_chunks() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"history\r\n", &mut Vec::new());
        terminal.process(b"\x1b[?104", &mut Vec::new());
        terminal.process(b"9h\x1b[Halternate", &mut Vec::new());
        assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "a");
    }

    #[test]
    fn rio_keeps_replies_from_device_status_queries() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        let mut events = Vec::new();
        terminal.process(b"\x1b[6n", &mut events);
        assert!(events.iter().any(|event| {
            matches!(event, super::VtEvent::Reply(reply) if reply.as_str() == "\x1b[1;1R")
        }));
    }

    #[test]
    fn rio_answers_dynamic_background_queries() {
        let mut terminal = TerminalCore::new_with_host_appearance(
            24,
            80,
            100,
            [
                rio_vt::config::colors::ColorRgb {
                    r: 12,
                    g: 34,
                    b: 56,
                },
                rio_vt::config::colors::ColorRgb {
                    r: 78,
                    g: 90,
                    b: 123,
                },
                rio_vt::config::colors::ColorRgb {
                    r: 200,
                    g: 201,
                    b: 202,
                },
            ],
            None,
        );
        let mut events = Vec::new();

        terminal.process(b"\x1b]11;?\x07", &mut events);

        assert!(events.iter().any(|event| {
            matches!(
                event,
                super::VtEvent::Reply(reply)
                    if reply.as_str() == "\x1b]11;rgb:4e4e/5a5a/7b7b\x07"
            )
        }));
    }

    #[test]
    fn rio_delivers_replies_and_bells_without_forwarding_window_events() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        let mut events = Vec::new();
        terminal.process(b"\x1b]0;shell title\x07\x07\x1b[6n", &mut events);
        assert!(matches!(
            events.as_slice(),
            [super::VtEvent::Bell, super::VtEvent::Reply(reply)] if reply == "\x1b[1;1R"
        ));

        terminal.process(b"", &mut events);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn rio_maps_mouse_and_cursor_modes() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[?1002h\x1b[?1005h\x1b[?1h\x1b[?25l", &mut Vec::new());
        let screen = terminal.screen();
        assert_eq!(
            screen.mouse_protocol_mode(),
            super::MouseProtocolMode::ButtonMotion
        );
        assert_eq!(
            screen.mouse_protocol_encoding(),
            super::MouseProtocolEncoding::Utf8
        );
        assert!(screen.application_cursor());
        assert!(screen.hide_cursor());
    }

    #[test]
    fn rio_tracks_application_key_encoding_protocols() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        assert_eq!(
            terminal.screen().key_protocol(),
            super::TerminalKeyProtocol::Legacy
        );

        terminal.process(b"\x1b[>1u", &mut Vec::new());
        assert_eq!(
            terminal.screen().key_protocol(),
            super::TerminalKeyProtocol::Kitty
        );

        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process(b"\x1b[>4;2m", &mut Vec::new());
        assert_eq!(
            terminal.screen().key_protocol(),
            super::TerminalKeyProtocol::ModifyOtherKeys(2)
        );
    }

    #[test]
    fn rio_clamps_zero_dimensions_to_one_cell() {
        let mut terminal = TerminalCore::new(0, 0, 100);
        assert_eq!(
            terminal.screen().size(),
            super::Size {
                width: 1,
                height: 1
            }
        );
        terminal.set_size(0, 0);
        assert_eq!(
            terminal.screen().size(),
            super::Size {
                width: 1,
                height: 1
            }
        );
    }

    #[test]
    fn rio_suppresses_only_initial_line_breaks_after_control_sequences() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.suppress_initial_line_breaks();
        terminal.process(b"\x1b[0m\r\n\r\nlogin: ", &mut Vec::new());
        assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "l");
        terminal.process(b"\r\n\r\nnext", &mut Vec::new());
        assert_eq!(terminal.screen().cell(2, 0).unwrap().contents(), "n");
    }

    #[test]
    fn rio_selection_uses_row_relative_coordinates() {
        let mut terminal = TerminalCore::new(4, 20, 20);
        terminal.process(b"one\r\ntwo\r\nthree", &mut Vec::new());
        assert_eq!(terminal.screen().get_selected_text(0, 0, 2, 1), "one\ntwo");
    }

    #[test]
    fn rio_decodes_bulk_utf8_and_a_split_wide_character() {
        let mut terminal = TerminalCore::new(2, 160, 20);
        let text = "éΩ".repeat(64);
        terminal.process(text.as_bytes(), &mut Vec::new());
        assert_eq!(terminal.screen().get_selected_text(0, 0, 127, 0), text);

        let wide = "界".as_bytes();
        terminal.process(&wide[..2], &mut Vec::new());
        assert_eq!(terminal.screen().cursor_position(), (0, 128));
        terminal.process(&wide[2..], &mut Vec::new());
        assert_eq!(terminal.screen().cell(0, 128).unwrap().contents(), "界");
        assert_eq!(terminal.screen().cursor_position(), (0, 130));
    }

    #[test]
    fn browser_mode_uses_rio_cursor_and_selection_semantics() {
        let mut terminal = TerminalCore::new(4, 20, 20);
        terminal.process(b"one\r\ntwo\r\nthree", &mut Vec::new());
        terminal.set_browser_mode(true);
        assert!(terminal.browser_mode());

        terminal.browser_motion(BrowserMotion::First);
        assert_eq!(terminal.screen().cursor_position(), (2, 0));
        terminal.begin_browser_selection(BrowserSelectionKind::Character);
        terminal.browser_motion(BrowserMotion::Right);
        terminal.browser_motion(BrowserMotion::Right);
        assert_eq!(terminal.browser_selection_text().as_deref(), Some("thr"));

        terminal.clear_browser_selection();
        assert_eq!(terminal.browser_selection_text(), None);
        let browser_position = terminal.screen().cursor_position();
        terminal.set_browser_mode(false);
        assert!(!terminal.browser_mode());
        terminal.set_browser_mode(true);
        assert_eq!(terminal.screen().cursor_position(), browser_position);
    }

    #[test]
    fn internal_browser_cursor_toggle_does_not_become_an_application_request() {
        let mut terminal = TerminalCore::new(4, 20, 20);
        assert_eq!(terminal.screen().cursor_style(), None);

        terminal.set_browser_mode(true);
        terminal.set_browser_mode(false);
        assert_eq!(terminal.screen().cursor_style(), None);

        // Kitty exposes its configured default cursor through the host, not as
        // an explicit application style. A later PTY write must not turn Rio's
        // internal VI notification into a steady-block override.
        terminal.process(b"prompt", &mut Vec::new());
        assert_eq!(terminal.screen().cursor_style(), None);
    }

    #[test]
    fn browser_selection_range_tracks_scrollback_coordinates() {
        let mut terminal = TerminalCore::new(2, 20, 20);
        terminal.process(b"one\r\ntwo\r\nthree", &mut Vec::new());
        terminal.set_browser_mode(true);
        terminal.browser_motion(BrowserMotion::First);
        terminal.begin_browser_selection(BrowserSelectionKind::Line);
        terminal.set_scrollback(1);
        let range = terminal
            .screen()
            .browser_selection_range()
            .expect("line selection range");
        assert!(range.start_row <= range.end_row);
        assert!(range.contains(range.start_row.max(0) as u16, range.start_col));
    }

    #[test]
    fn rio_idle_viewport_updates_do_not_accumulate_events() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        terminal.process("history\r\n".repeat(100).as_bytes(), &mut Vec::new());

        // A quiet shell receives no PTY output to drain events between frames.
        // Exercise both the live viewport and a scrolled viewport, as the panel
        // restores the live position after every draw.
        for offset in [0, 50] {
            for _ in 0..10_000 {
                terminal.set_scrollback(offset);
                let _ = terminal.screen().cell(0, 0);
                terminal.set_scrollback(0);
            }
        }

        let state = terminal.listener.state.lock().unwrap();
        let pending = &state.events;
        assert!(
            pending.is_empty(),
            "idle rendering retained {} events ({} bytes before allocator overhead)",
            pending.len(),
            std::mem::size_of_val(pending.as_slice())
        );
        assert_eq!(pending.capacity(), 0);
    }
}
