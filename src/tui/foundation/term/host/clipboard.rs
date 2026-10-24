//! OSC 52 clipboard output to the outer terminal.
//!
//! The TUI already owns the user's terminal output.  Sending the selection
//! through that output keeps clipboard integration independent of a desktop
//! Wayland/X11 session and also works when Lasper is running through a remote
//! terminal.  The terminal emulator (or an intermediary such as tmux) decides
//! whether to accept the sequence.

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardTarget {
    Clipboard,
    Primary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Direct,
    Tmux,
    Screen,
}

impl ClipboardTarget {
    const fn parameter(self) -> &'static [u8] {
        match self {
            Self::Clipboard => b"c",
            Self::Primary => b"p",
        }
    }
}

/// Ask the outer terminal to store `text` in the requested selection.
pub(crate) fn set_clipboard(target: ClipboardTarget, text: &str) -> io::Result<()> {
    write_clipboard(
        &mut io::stdout().lock(),
        target,
        text,
        transport_from_environment(),
    )
}

fn write_clipboard(
    output: &mut impl Write,
    target: ClipboardTarget,
    text: &str,
    transport: Transport,
) -> io::Result<()> {
    let mut encoded = String::with_capacity(encoded_len(text.len()));
    encode_base64(text.as_bytes(), &mut encoded);

    let sequence = sequence(target, &encoded, transport);
    output.write_all(&sequence)?;
    output.flush()
}

fn transport_from_environment() -> Transport {
    if std::env::var_os("TMUX").is_some_and(|value| !value.is_empty()) {
        Transport::Tmux
    } else if std::env::var_os("STY").is_some_and(|value| !value.is_empty()) {
        Transport::Screen
    } else {
        Transport::Direct
    }
}

fn sequence(target: ClipboardTarget, encoded: &str, transport: Transport) -> Vec<u8> {
    let mut osc = Vec::with_capacity(8 + encoded.len());
    osc.extend_from_slice(b"\x1b]52;");
    osc.extend_from_slice(target.parameter());
    osc.extend_from_slice(b";");
    osc.extend_from_slice(encoded.as_bytes());
    osc.push(0x07);

    match transport {
        Transport::Direct => osc,
        Transport::Tmux => {
            // tmux's DCS passthrough escapes every ESC in the enclosed
            // sequence, then restores the outer DCS terminator.
            let mut wrapped = Vec::with_capacity(osc.len() + 16);
            wrapped.extend_from_slice(b"\x1bPtmux;");
            for byte in osc {
                if byte == 0x1b {
                    wrapped.push(0x1b);
                }
                wrapped.push(byte);
            }
            wrapped.extend_from_slice(b"\x1b\\");
            wrapped
        }
        Transport::Screen => {
            let mut wrapped = Vec::with_capacity(osc.len() + 8);
            wrapped.extend_from_slice(b"\x1bP");
            wrapped.extend_from_slice(&osc);
            wrapped.extend_from_slice(b"\x1b\\");
            wrapped
        }
    }
}

const fn encoded_len(input_len: usize) -> usize {
    input_len.saturating_add(2) / 3 * 4
}

fn encode_base64(input: &[u8], output: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
        output.push(match second {
            Some(second) => {
                ALPHABET[((second & 0x0f) << 2 | third.unwrap_or(0) >> 6) as usize] as char
            }
            None => '=',
        });
        output.push(match third {
            Some(third) => ALPHABET[(third & 0x3f) as usize] as char,
            None => '=',
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_base64, encoded_len, sequence, write_clipboard, ClipboardTarget, Transport,
    };

    fn encode(input: &str) -> String {
        let mut output = String::with_capacity(encoded_len(input.len()));
        encode_base64(input.as_bytes(), &mut output);
        output
    }

    #[test]
    fn encodes_standard_vectors() {
        assert_eq!(encode(""), "");
        assert_eq!(encode("f"), "Zg==");
        assert_eq!(encode("fo"), "Zm8=");
        assert_eq!(encode("foo"), "Zm9v");
        assert_eq!(encode("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encoded_length_is_four_thirds_rounded_up() {
        assert_eq!(encoded_len(0), 0);
        assert_eq!(encoded_len(1), 4);
        assert_eq!(encoded_len(2), 4);
        assert_eq!(encoded_len(3), 4);
        assert_eq!(encoded_len(4), 8);
    }

    #[test]
    fn a_clipboard_write_emits_one_encoded_request() {
        let mut output = Vec::new();
        write_clipboard(
            &mut output,
            ClipboardTarget::Clipboard,
            "foo",
            Transport::Direct,
        )
        .unwrap();
        assert_eq!(output, b"\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn wraps_osc52_for_tmux_and_screen() {
        assert_eq!(
            sequence(ClipboardTarget::Primary, "Zg==", Transport::Tmux),
            b"\x1bPtmux;\x1b\x1b]52;p;Zg==\x07\x1b\\"
        );
        assert_eq!(
            sequence(ClipboardTarget::Primary, "Zg==", Transport::Screen),
            b"\x1bP\x1b]52;p;Zg==\x07\x1b\\"
        );
    }
}
