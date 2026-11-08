use core::fmt::{self, Write};
use serde::{Serialize, Serializer};
use serde_json_core::str::{EscapedStr, EscapedStringFragment};

use super::MIN_HELPER_CONTROL_FRAME_BYTES;
use crate::{ControlLimits, PayloadCodecError};

pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Clone, Copy, Debug)]
enum OutputBytes<'a> {
    Raw(&'a [u8]),
    Encoded(EscapedStr<'a>),
}

/// A bounded byte stream, with truncation and EOF recorded independently.
/// Non-UTF-8 command output is base64 on the wire, never a diagnostic string.
#[derive(Clone, Copy, Debug)]
pub struct OutputStream<'a> {
    bytes: OutputBytes<'a>,
    length: usize,
    truncated: bool,
    complete: bool,
}

impl<'a> OutputStream<'a> {
    pub fn new(
        bytes: &'a [u8],
        truncated: bool,
        complete: bool,
    ) -> Result<Self, PayloadCodecError> {
        if bytes.len() > MAX_CAPTURED_OUTPUT_BYTES {
            return Err(PayloadCodecError::PayloadTooLarge);
        }
        Ok(Self {
            bytes: OutputBytes::Raw(bytes),
            length: bytes.len(),
            truncated,
            complete,
        })
    }

    pub const fn len(&self) -> usize {
        self.length
    }
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
    pub const fn complete(&self) -> bool {
        self.complete
    }

    pub fn write_into(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        if output.len() < self.length {
            return Err(PayloadCodecError::OutputTooSmall);
        }
        match self.bytes {
            OutputBytes::Raw(bytes) => output[..bytes.len()].copy_from_slice(bytes),
            OutputBytes::Encoded(value) => {
                decode_base64(value, Some(output))?;
            }
        }
        Ok(self.length)
    }

    pub(super) fn decoded(
        value: EscapedStr<'a>,
        truncated: bool,
        complete: bool,
    ) -> Result<Self, PayloadCodecError> {
        let length = decode_base64(value, None)?;
        Ok(Self {
            bytes: OutputBytes::Encoded(value),
            length,
            truncated,
            complete,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CapturedOutput<'a> {
    stdout: OutputStream<'a>,
    stderr: OutputStream<'a>,
}

impl<'a> CapturedOutput<'a> {
    pub fn new(
        stdout: OutputStream<'a>,
        stderr: OutputStream<'a>,
    ) -> Result<Self, PayloadCodecError> {
        if stdout.len() + stderr.len() > MAX_CAPTURED_OUTPUT_BYTES {
            return Err(PayloadCodecError::PayloadTooLarge);
        }
        Ok(Self { stdout, stderr })
    }

    pub const fn stdout(&self) -> &OutputStream<'a> {
        &self.stdout
    }
    pub const fn stderr(&self) -> &OutputStream<'a> {
        &self.stderr
    }

    /// Reserve a full status envelope, then account for base64 expansion and
    /// independent padding in stdout/stderr. The actual frame is checked again
    /// after encoding; capture never consumes the space needed for exit facts.
    pub const fn budget(limits: ControlLimits) -> usize {
        let available = limits
            .max_control_frame_bytes()
            .saturating_sub(MIN_HELPER_CONTROL_FRAME_BYTES)
            .saturating_sub(4) as usize;
        let bytes = available / 4 * 3;
        if bytes < MAX_CAPTURED_OUTPUT_BYTES {
            bytes
        } else {
            MAX_CAPTURED_OUTPUT_BYTES
        }
    }
}

pub(super) struct EncodedOutput<'a>(pub(super) OutputStream<'a>);

impl Serialize for EncodedOutput<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.bytes {
            OutputBytes::Raw(bytes) => serializer.collect_str(&Base64(bytes)),
            OutputBytes::Encoded(value) => value.serialize(serializer),
        }
    }
}

struct Base64<'a>(&'a [u8]);

impl fmt::Display for Base64<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // collect_str writes these four characters straight into the caller's
        // JSON buffer; an additional base64-sized allocation is not needed.
        for chunk in self.0.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            formatter.write_char(ALPHABET[(a >> 2) as usize] as char)?;
            formatter.write_char(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char)?;
            formatter.write_char(if chunk.len() >= 2 {
                ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
            } else {
                '='
            })?;
            formatter.write_char(if chunk.len() == 3 {
                ALPHABET[(c & 63) as usize] as char
            } else {
                '='
            })?;
        }
        Ok(())
    }
}

fn decode_base64(
    value: EscapedStr<'_>,
    mut output: Option<&mut [u8]>,
) -> Result<usize, PayloadCodecError> {
    let mut quartet = [0u8; 4];
    let mut count = 0;
    let mut written = 0;
    let mut padded = false;
    for fragment in value.fragments() {
        let fragment = fragment.map_err(|_| PayloadCodecError::InvalidRequest)?;
        let mut character = [0; 4];
        let text = match fragment {
            EscapedStringFragment::NotEscaped(text) => text,
            EscapedStringFragment::Escaped(ch) => ch.encode_utf8(&mut character),
        };
        for &byte in text.as_bytes() {
            if padded {
                return Err(PayloadCodecError::InvalidRequest);
            }
            quartet[count] = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => 64,
                _ => return Err(PayloadCodecError::InvalidRequest),
            };
            count += 1;
            if count != 4 {
                continue;
            }
            let [a, b, c, d] = quartet;
            if a == 64 || b == 64 {
                return Err(PayloadCodecError::InvalidRequest);
            }
            let bytes = match (c, d) {
                (64, 64) if b & 15 == 0 => 1,
                (0..=63, 64) if c & 3 == 0 => 2,
                (0..=63, 0..=63) => 3,
                _ => return Err(PayloadCodecError::InvalidRequest),
            };
            if written + bytes > MAX_CAPTURED_OUTPUT_BYTES {
                return Err(PayloadCodecError::PayloadTooLarge);
            }
            if let Some(buffer) = output.as_deref_mut() {
                if buffer.len() < written + bytes {
                    return Err(PayloadCodecError::OutputTooSmall);
                }
                let decoded = [(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d];
                buffer[written..written + bytes].copy_from_slice(&decoded[..bytes]);
            }
            written += bytes;
            count = 0;
            padded = bytes != 3;
        }
    }
    if count != 0 {
        return Err(PayloadCodecError::InvalidRequest);
    }
    Ok(written)
}
