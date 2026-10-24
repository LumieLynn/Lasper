//! Apply embedded cursor requests to the outer terminal and restore its style.

use crossterm::{cursor::SetCursorStyle, execute};
use std::io::Write;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

/// Cursor appearance requested by an application in an embedded terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum CursorStyle {
    BlinkingBlock = 1,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

impl CursorStyle {
    pub(super) fn from_parameter(parameter: u8) -> Option<Self> {
        match parameter {
            1 => Some(Self::BlinkingBlock),
            2 => Some(Self::SteadyBlock),
            3 => Some(Self::BlinkingUnderline),
            4 => Some(Self::SteadyUnderline),
            5 => Some(Self::BlinkingBar),
            6 => Some(Self::SteadyBar),
            _ => None,
        }
    }

    fn command(self) -> SetCursorStyle {
        match self {
            Self::BlinkingBlock => SetCursorStyle::BlinkingBlock,
            Self::SteadyBlock => SetCursorStyle::SteadyBlock,
            Self::BlinkingUnderline => SetCursorStyle::BlinkingUnderScore,
            Self::SteadyUnderline => SetCursorStyle::SteadyUnderScore,
            Self::BlinkingBar => SetCursorStyle::BlinkingBar,
            Self::SteadyBar => SetCursorStyle::SteadyBar,
        }
    }
}

/// A captured host cursor can inherit the terminal's configuration rather than
/// have an explicit shape. That state must be restored without choosing a shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostCursorStyle {
    TerminalDefault,
    Explicit(CursorStyle),
}

impl HostCursorStyle {
    fn stored_parameter(self) -> u8 {
        match self {
            Self::TerminalDefault => 7,
            Self::Explicit(style) => style as u8,
        }
    }

    fn from_stored_parameter(parameter: u8) -> Option<Self> {
        match parameter {
            7 => Some(Self::TerminalDefault),
            _ => CursorStyle::from_parameter(parameter).map(Self::Explicit),
        }
    }

    fn command(self) -> SetCursorStyle {
        match self {
            Self::TerminalDefault => SetCursorStyle::DefaultUserShape,
            Self::Explicit(style) => style.command(),
        }
    }
}

fn dcs_responses(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.iter().enumerate().filter_map(move |(offset, byte)| {
        let start = match byte {
            0x90 => offset + 1,
            0x1b if bytes.get(offset + 1) == Some(&b'P') => offset + 2,
            _ => return None,
        };
        let rest = &bytes[start..];
        let end = rest.iter().enumerate().position(|(offset, byte)| {
            *byte == 0x9c || (*byte == 0x1b && rest.get(offset + 1) == Some(&b'\\'))
        })?;
        Some(&rest[..end])
    })
}

fn terminal_identity_response(bytes: &[u8]) -> Option<&[u8]> {
    dcs_responses(bytes).find_map(|body| body.strip_prefix(b">|"))
}

pub(super) fn has_terminal_identity(bytes: &[u8]) -> bool {
    terminal_identity_response(bytes).is_some()
}

/// Decode the host's DECRQSS cursor response and preserve its default semantics.
pub(super) fn decode_query_response(bytes: &[u8]) -> Option<Option<HostCursorStyle>> {
    let is_kitty = terminal_identity_response(bytes)
        .is_some_and(|identity| identity.starts_with(b"kitty(") && identity.ends_with(b")"));
    dcs_responses(bytes).find_map(|body| {
        if body == b"0$r" {
            return Some(None);
        }
        let parameter = body
            .strip_prefix(b"1$r")?
            .strip_suffix(b" q")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<u8>().ok())?;
        // Kitty uses 1 q to report its configured shape when no override is active.
        if is_kitty && parameter == 1 {
            Some(Some(HostCursorStyle::TerminalDefault))
        } else {
            Some(CursorStyle::from_parameter(parameter.max(1)).map(HostCursorStyle::Explicit))
        }
    })
}

#[derive(Default)]
struct HostCursorState {
    original: AtomicU8,
    current: AtomicU8,
}

/// Owns cursor-style output and restoration for the process terminal.
/// Clones share restoration state with the normal and panic cleanup paths.
#[derive(Clone, Default)]
pub(crate) struct HostCursor {
    state: Arc<HostCursorState>,
}

impl HostCursor {
    pub(crate) fn set_original(&mut self, style: Option<HostCursorStyle>) {
        self.state.original.store(
            style.map_or(0, HostCursorStyle::stored_parameter),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn apply(
        &mut self,
        output: &mut impl Write,
        style: Option<CursorStyle>,
    ) -> std::io::Result<()> {
        // A cursor override is reversible only when the host cursor style was
        // captured successfully.  Sending an application style without a
        // known restore value would leave the outer terminal in an arbitrary
        // state after the embedded session exits.
        if style.is_some() && self.state.original.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let parameter = style.map_or(0, |style| style as u8);
        if self.state.current.load(Ordering::Relaxed) == parameter {
            return Ok(());
        }

        let Some(style) = style else {
            return self.restore(output);
        };
        execute!(output, style.command())?;
        self.state.current.store(parameter, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn restore(&self, output: &mut impl Write) -> std::io::Result<()> {
        if self.state.current.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let Some(original) =
            HostCursorStyle::from_stored_parameter(self.state.original.load(Ordering::Relaxed))
        else {
            return Ok(());
        };
        execute!(output, original.command())?;
        self.state.current.store(0, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorStyle, HostCursor, HostCursorStyle};
    use crate::tui::foundation::term::TerminalCore;

    #[test]
    fn cursor_query_requires_a_complete_successful_status_response() {
        assert_eq!(
            super::decode_query_response(b"noise\x1b]11;rgb:00/00/00\x07\x1bP1$r6 q\x1b\\"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::SteadyBar)))
        );
        assert_eq!(super::decode_query_response(b"\x1bP1$r5 q"), None);
        assert_eq!(super::decode_query_response(b"\x1bP0$r\x1b\\"), Some(None));
        assert_eq!(
            super::decode_query_response(b"\x1bP1$r99 q\x1b\\"),
            Some(None)
        );
        assert_eq!(
            super::decode_query_response(b"\x901$r4 q\x9c"),
            Some(Some(HostCursorStyle::Explicit(
                CursorStyle::SteadyUnderline
            )))
        );
    }

    #[test]
    fn kitty_default_cursor_is_restored_after_an_embedded_alternate_screen() {
        let response = b"\x1bP>|kitty(0.45.0)\x1b\\\x1bP1$r1 q\x1b\\";
        let original = super::decode_query_response(response).unwrap();
        assert_eq!(original, Some(HostCursorStyle::TerminalDefault));
        let mut cursor = HostCursor::default();
        cursor.set_original(original);
        let mut terminal = TerminalCore::new(24, 80, 100);
        let mut output = Vec::new();
        terminal.process(b"\x1b[?1049h\x1b[6 q", &mut Vec::new());
        cursor
            .apply(&mut output, terminal.screen().cursor_style())
            .unwrap();
        terminal.process(b"\x1b[2 q\x1b[?1049l", &mut Vec::new());
        cursor
            .apply(&mut output, terminal.screen().cursor_style())
            .unwrap();
        cursor.restore(&mut output).unwrap();
        assert_eq!(output, b"\x1b[6 q\x1b[0 q");

        assert_eq!(
            super::decode_query_response(b"\x1bP>|tmux 3.5\x1b\\\x1bP1$r1 q\x1b\\"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)))
        );
        assert_eq!(
            super::decode_query_response(b"\x1bP1$r0 q\x1b\\\x90>|kitty(0.45.0)\x9c"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)))
        );
    }

    #[test]
    fn kitty_blinking_request_does_not_choose_a_block_cursor() {
        let mut terminal = TerminalCore::new(24, 80, 100);
        let mut cursor = HostCursor::default();
        cursor.set_original(Some(HostCursorStyle::TerminalDefault));
        let mut output = Vec::new();

        terminal.process(b"\x1b[?12l", &mut Vec::new());
        cursor
            .apply(&mut output, terminal.screen().cursor_style())
            .unwrap();
        assert!(!output.windows(5).any(|bytes| bytes == b"\x1b[2 q"));
    }

    #[test]
    fn changes_style_only_when_the_rendered_cursor_changes() {
        let mut cursor = HostCursor::default();
        let mut output = Vec::new();
        cursor.set_original(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)));
        cursor.apply(&mut output, None).unwrap();
        assert!(output.is_empty());

        cursor
            .apply(&mut output, Some(CursorStyle::SteadyBar))
            .unwrap();
        cursor
            .apply(&mut output, Some(CursorStyle::SteadyBar))
            .unwrap();
        assert_eq!(output, b"\x1b[6 q");

        cursor
            .apply(&mut output, Some(CursorStyle::BlinkingUnderline))
            .unwrap();
        cursor.apply(&mut output, None).unwrap();
        cursor.apply(&mut output, None).unwrap();
        assert_eq!(output, b"\x1b[6 q\x1b[3 q\x1b[1 q");
    }

    #[test]
    fn failed_output_does_not_commit_the_cursor_state() {
        let mut cursor = HostCursor::default();
        cursor.set_original(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)));
        let mut buffer = [];
        let mut full = std::io::Cursor::new(buffer.as_mut_slice());
        assert!(cursor
            .apply(&mut full, Some(CursorStyle::SteadyBar))
            .is_err());
        let mut output = Vec::new();
        cursor
            .apply(&mut output, Some(CursorStyle::SteadyBar))
            .unwrap();
        assert_eq!(output, b"\x1b[6 q");
    }

    #[test]
    fn cleanup_restores_the_captured_host_style_only_after_an_override() {
        let mut cursor = HostCursor::default();
        let cleanup = cursor.clone();
        cursor.set_original(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBar)));
        let mut output = Vec::new();
        cursor.apply(&mut output, None).unwrap();
        cleanup.restore(&mut output).unwrap();
        assert!(output.is_empty());

        cursor
            .apply(&mut output, Some(CursorStyle::SteadyUnderline))
            .unwrap();
        cleanup.restore(&mut output).unwrap();
        cleanup.restore(&mut output).unwrap();
        assert_eq!(output, b"\x1b[4 q\x1b[5 q");

        cursor
            .apply(&mut output, Some(CursorStyle::SteadyBlock))
            .unwrap();
        cursor.apply(&mut output, None).unwrap();
        assert!(output.ends_with(b"\x1b[2 q\x1b[5 q"));
    }

    #[test]
    fn unknown_host_style_does_not_receive_an_unrestorable_override() {
        let mut cursor = HostCursor::default();
        let mut output = Vec::new();
        cursor
            .apply(&mut output, Some(CursorStyle::SteadyBar))
            .unwrap();
        cursor.apply(&mut output, None).unwrap();
        assert!(output.is_empty());
    }
}
