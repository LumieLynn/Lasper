use super::PayloadCodecError;
use serde_json_core::str::{EscapedStr, EscapedStringFragment};

/// Decodes borrowed JSON strings without copying their contents. Surrogate
/// pairs form one scalar value and count as that scalar's UTF-8 bytes.
pub(crate) struct StringFragments<'a>(&'a str);

pub(crate) fn fragments(value: EscapedStr<'_>) -> StringFragments<'_> {
    StringFragments(value.0)
}

impl<'a> Iterator for StringFragments<'a> {
    type Item = Result<EscapedStringFragment<'a>, PayloadCodecError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let remaining = self.0;
        self.0 = "";
        Some((|| {
            if let Some(rest) = remaining.strip_prefix('\\') {
                let (&escape, mut rest) = rest
                    .as_bytes()
                    .split_first()
                    .ok_or(PayloadCodecError::InvalidJson)?;
                let ch = match escape {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let (first, tail) = hex_quad(rest)?;
                        rest = tail;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            let tail = rest
                                .strip_prefix(b"\\u")
                                .ok_or(PayloadCodecError::InvalidJson)?;
                            let (second, tail) = hex_quad(tail)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return Err(PayloadCodecError::InvalidJson);
                            }
                            rest = tail;
                            0x10000 + ((u32::from(first) - 0xd800) << 10) + u32::from(second)
                                - 0xdc00
                        } else {
                            u32::from(first)
                        };
                        char::from_u32(scalar).ok_or(PayloadCodecError::InvalidJson)?
                    }
                    _ => return Err(PayloadCodecError::InvalidJson),
                };
                // Only ASCII escape bytes were consumed from this valid str.
                // Slicing avoids rescanning the remaining UTF-8 on every escape.
                self.0 = &remaining[remaining.len() - rest.len()..];
                Ok(EscapedStringFragment::Escaped(ch))
            } else {
                let end = remaining.find('\\').unwrap_or(remaining.len());
                let (text, rest) = remaining.split_at(end);
                if text.bytes().any(|byte| byte < 0x20) {
                    return Err(PayloadCodecError::InvalidJson);
                }
                self.0 = rest;
                Ok(EscapedStringFragment::NotEscaped(text))
            }
        })())
    }
}

fn hex_quad(bytes: &[u8]) -> Result<(u16, &[u8]), PayloadCodecError> {
    let digits = bytes.get(..4).ok_or(PayloadCodecError::InvalidJson)?;
    let mut value = 0;
    for byte in digits {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(PayloadCodecError::InvalidJson),
        };
        value = (value << 4) | u16::from(digit);
    }
    Ok((value, &bytes[4..]))
}
