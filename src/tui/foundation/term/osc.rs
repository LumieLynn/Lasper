//! Consume metadata which has no representation in an embedded terminal panel.
//!
//! OSC 1 changes a window's icon label; OSC 3008 describes systemd execution
//! contexts. Neither changes the terminal grid. Rio does not recognize them,
//! so consume these two commands here instead of filtering its warning logs.
//! All other commands reach Rio unchanged, including unsupported commands.

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Command,
    String {
        osc: bool,
        escaped: bool,
    },
    Discard,
    DiscardEscape,
}

#[derive(Default)]
pub(super) struct OscMetadataFilter {
    state: State,
    // ESC ] 3008 ; is the longest prefix we need to recognize. Never buffer
    // a payload: a split, very long or unterminated OSC must not grow memory.
    prefix: [u8; 7],
    prefix_len: usize,
}

impl OscMetadataFilter {
    pub(super) fn push(&mut self, mut bytes: &[u8], output: &mut Vec<u8>) {
        while let Some((&byte, rest)) = bytes.split_first() {
            if matches!(self.state, State::Ground) {
                let plain = bytes
                    .iter()
                    .position(|byte| *byte == 0x1b)
                    .unwrap_or(bytes.len());
                output.extend_from_slice(&bytes[..plain]);
                bytes = &bytes[plain..];
                if bytes.is_empty() {
                    break;
                }
                self.prefix[0] = 0x1b;
                self.prefix_len = 1;
                self.state = State::Escape;
                bytes = &bytes[1..];
            } else {
                self.push_byte(byte, output);
                bytes = rest;
            }
        }
    }

    fn push_byte(&mut self, byte: u8, output: &mut Vec<u8>) {
        match self.state {
            State::Ground => unreachable!("ground runs are handled in push"),
            State::Escape if byte == b']' => {
                self.prefix[1] = byte;
                self.prefix_len = 2;
                self.state = State::Command;
            }
            State::Escape if byte == 0x1b => output.push(byte),
            State::Escape => {
                self.flush_prefix(output);
                output.push(byte);
                self.state = match byte {
                    b'P' | b'X' | b'^' | b'_' => State::String {
                        osc: false,
                        escaped: false,
                    },
                    _ => State::Ground,
                };
            }
            State::Command if byte.is_ascii_digit() && self.prefix_len < 6 => {
                self.prefix[self.prefix_len] = byte;
                self.prefix_len += 1;
            }
            State::Command
                if byte == b';' && matches!(&self.prefix[2..self.prefix_len], b"1" | b"3008") =>
            {
                self.prefix_len = 0;
                self.state = State::Discard;
            }
            State::Command => {
                self.flush_prefix(output);
                if byte == 0x1b {
                    self.prefix[0] = byte;
                    self.prefix_len = 1;
                    self.state = State::Escape;
                } else {
                    output.push(byte);
                    self.state = if matches!(byte, 0x07 | 0x18 | 0x1a) {
                        State::Ground
                    } else {
                        State::String {
                            osc: true,
                            escaped: false,
                        }
                    };
                }
            }
            State::String { osc, escaped } => {
                output.push(byte);
                self.state = if (osc && byte == 0x07)
                    || matches!(byte, 0x18 | 0x1a)
                    || (escaped && byte == b'\\')
                {
                    State::Ground
                } else {
                    State::String {
                        osc,
                        escaped: byte == 0x1b,
                    }
                };
            }
            State::Discard => match byte {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::DiscardEscape,
                0x18 | 0x1a => {
                    output.push(byte);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::DiscardEscape if byte == b'\\' => self.state = State::Ground,
            State::DiscardEscape => {
                // ESC also cancels an OSC without ST. Preserve the following
                // escape command rather than swallowing it with the metadata.
                self.prefix[0] = 0x1b;
                self.prefix_len = 1;
                self.state = State::Escape;
                self.push_byte(byte, output);
            }
        }
    }

    fn flush_prefix(&mut self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.prefix[..self.prefix_len]);
        self.prefix_len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::OscMetadataFilter;

    fn check_chunk_boundaries(input: &[u8], expected: &[u8]) {
        for chunk_size in 1..=input.len() {
            let mut filter = OscMetadataFilter::default();
            let mut output = Vec::new();
            for chunk in input.chunks(chunk_size) {
                filter.push(chunk, &mut output);
            }
            assert_eq!(output, expected, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn consumes_only_icon_labels_and_execution_contexts() {
        check_chunk_boundaries(
            b"before\x1b]1;label\x07\x1b]3008;start=id;user=root\x1b\\after\x1b]3008;end=id\x07!",
            b"beforeafter!",
        );
        let other_commands = b"\x1b]10;?\x07\x1b]11;rgb:11/22/33\x1b\\\x1b]8;;https://example.invalid\x1b\\link\x1b]8;;\x1b\\\x1b]9999;unknown\x07";
        check_chunk_boundaries(other_commands, other_commands);
    }

    #[test]
    fn does_not_filter_metadata_prefixes_inside_other_control_strings() {
        let input = b"\x1bPtmux;\x1b\x1b]1;title\x07\x1b\\\x1b_Gpayload\x1b]3008;data\x1b\\";
        check_chunk_boundaries(input, input);
    }

    #[test]
    fn preserves_cancellation_and_following_escape_commands() {
        check_chunk_boundaries(
            b"\x1b]3008;cancel\x18A\x1b]1;cancel\x1aB\x1b]3008;cancel\x1b[31mC",
            b"\x18A\x1aB\x1b[31mC",
        );
    }

    #[test]
    fn unfinished_metadata_never_buffers_its_payload() {
        let mut filter = OscMetadataFilter::default();
        let mut output = Vec::new();
        filter.push(b"\x1b]3008;start=", &mut output);
        for _ in 0..1000 {
            filter.push(&[b'x'; 1024], &mut output);
        }
        assert_eq!(filter.prefix_len, 0);
        assert!(output.is_empty());
        filter.push(b"\x07ready", &mut output);
        assert_eq!(output, b"ready");
    }
}
