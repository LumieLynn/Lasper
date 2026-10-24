//! Cursor control at the boundary between the embedded terminal and its host.

mod host;

pub(crate) use host::{CursorStyle, HostCursor, HostCursorStyle};

use rio_vt::ansi::CursorShape;
use rio_vt::performer::parser::{Params, Parser, Perform};

/// Tracks whether the embedded program currently owns the host cursor shape.
///
/// Rio exposes a combined blink/shape notification. Parsing the small set of
/// cursor control sequences here keeps that notification out of the policy
/// decision while Rio remains the source of the displayed shape and blink bit.
#[derive(Default)]
pub(super) struct CursorRequestTracker {
    parser: Parser,
    style_requested: bool,
    host_shape_known: bool,
}

impl CursorRequestTracker {
    pub(super) fn advance(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub(super) fn set_host_shape_known(&mut self) {
        self.host_shape_known = true;
    }

    pub(super) fn style_requested(&self) -> bool {
        self.style_requested
    }

    pub(super) fn restore_style_requested(&mut self, requested: bool) {
        self.style_requested = requested;
    }

    pub(super) fn style(&self, shape: CursorShape, blinking: bool) -> Option<CursorStyle> {
        if !self.style_requested {
            return None;
        }
        match (shape, blinking) {
            (CursorShape::Hidden, _) => None,
            (CursorShape::Block, true) => Some(CursorStyle::BlinkingBlock),
            (CursorShape::Block, false) => Some(CursorStyle::SteadyBlock),
            (CursorShape::Underline, true) => Some(CursorStyle::BlinkingUnderline),
            (CursorShape::Underline, false) => Some(CursorStyle::SteadyUnderline),
            (CursorShape::Beam, true) => Some(CursorStyle::BlinkingBar),
            (CursorShape::Beam, false) => Some(CursorStyle::SteadyBar),
        }
    }
}

impl Perform for CursorRequestTracker {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }

        if action == 'q' && intermediates == b" " {
            let parameter = params
                .iter()
                .next()
                .and_then(|group| group.first())
                .copied();
            if matches!(parameter, None | Some(0)) {
                self.style_requested = false;
            } else if matches!(parameter, Some(1..=6)) {
                self.style_requested = true;
            }
        } else if matches!(action, 'h' | 'l')
            && intermediates == b"?"
            && params.iter().flatten().any(|parameter| *parameter == 12)
        {
            self.style_requested |= self.host_shape_known;
        } else if action == 'p' && intermediates == b"!" {
            self.style_requested = false;
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.style_requested = false;
        }
    }
}

pub(super) fn decode_query_response(bytes: &[u8]) -> Option<Option<host::HostCursorStyle>> {
    host::decode_query_response(bytes)
}

pub(super) fn has_terminal_identity(bytes: &[u8]) -> bool {
    host::has_terminal_identity(bytes)
}

#[cfg(test)]
mod tests {
    use super::CursorRequestTracker;

    fn advance(tracker: &mut CursorRequestTracker, bytes: &[u8]) -> bool {
        tracker.advance(bytes);
        tracker.style_requested()
    }

    #[test]
    fn identifies_explicit_shape_and_default_requests_across_chunks() {
        for (sequence, expected) in [
            (b"\x1b[1 q".as_slice(), true),
            (b"\x1b[2 q".as_slice(), true),
            (b"\x1b[3 q".as_slice(), true),
            (b"\x1b[4 q".as_slice(), true),
            (b"\x1b[5 q".as_slice(), true),
            (b"\x1b[6 q".as_slice(), true),
            (b"\x1b[0 q".as_slice(), false),
            (b"\x1b[ q".as_slice(), false),
            (b"\x1b[!p".as_slice(), false),
            (b"\x1bc".as_slice(), false),
        ] {
            for chunk_size in 1..=sequence.len() {
                let mut tracker = CursorRequestTracker::default();
                let requests = sequence
                    .chunks(chunk_size)
                    .map(|chunk| advance(&mut tracker, chunk))
                    .collect::<Vec<_>>();
                assert_eq!(requests.last(), Some(&expected));
            }
        }
    }

    #[test]
    fn blink_mode_changes_and_strings_are_not_shape_requests() {
        let mut tracker = CursorRequestTracker::default();
        for bytes in [
            b"\x1b]0;title [2 q\x07".as_slice(),
            b"\x1bPpayload [2 q\x1b\\".as_slice(),
        ] {
            assert!(!advance(&mut tracker, bytes));
        }
        advance(&mut tracker, b"\x1b[?12l");
        assert!(!tracker.style_requested());
        advance(&mut tracker, b"\x1b[2 q");
        assert!(tracker.style_requested());
        advance(&mut tracker, b"\x1b[?12h");
        assert!(tracker.style_requested());
        advance(&mut tracker, b"\x1b[0 q");
        assert!(!tracker.style_requested());
    }

    #[test]
    fn blinking_uses_captured_host_shape_without_overriding_unknown_shape() {
        let mut tracker = CursorRequestTracker::default();
        tracker.advance(b"\x1b[?12l");
        assert!(!tracker.style_requested());
        tracker.set_host_shape_known();
        tracker.advance(b"\x1b[?12h");
        assert!(tracker.style_requested());
        tracker.advance(b"\x1b[0 q");
        assert!(!tracker.style_requested());
        tracker.advance(b"\x1b[?12l");
        assert!(tracker.style_requested());
    }
}
