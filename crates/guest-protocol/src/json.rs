//! Bounded JSON payloads and checks for wire representations accepted too broadly
//! by Serde. Capacity is supplied separately for control and identity channels.

use serde::{de::DeserializeOwned, Serialize};
use serde_json_core::str::{EscapedStr, EscapedStringFragment};

mod strings;
pub(crate) use strings::{fragments, StringFragments};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadCodecError {
    PayloadTooLarge,
    OutputTooSmall,
    InvalidJson,
    InvalidLimits,
    InvalidRequest,
}

/// Serializes a control payload into caller-owned storage without allocating.
pub(crate) fn encode_json_payload<T: Serialize + ?Sized>(
    value: &T,
    output: &mut [u8],
) -> Result<usize, PayloadCodecError> {
    encode_bounded_json(value, output, crate::MAX_CONTROL_PAYLOAD_BYTES as usize)
}

pub(crate) fn encode_bounded_json<T: Serialize + ?Sized>(
    value: &T,
    output: &mut [u8],
    maximum: usize,
) -> Result<usize, PayloadCodecError> {
    let capacity = output.len().min(maximum);
    serde_json_core::to_slice(value, &mut output[..capacity]).map_err(|_| {
        if output.len() >= maximum {
            PayloadCodecError::PayloadTooLarge
        } else {
            PayloadCodecError::OutputTooSmall
        }
    })
}

/// Deserializes one bounded JSON payload.
pub(crate) fn decode_json_payload<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, PayloadCodecError> {
    decode_bounded_json(bytes, crate::MAX_CONTROL_PAYLOAD_BYTES as usize)
}

pub(crate) fn decode_bounded_json<T: DeserializeOwned>(
    bytes: &[u8],
    maximum: usize,
) -> Result<T, PayloadCodecError> {
    if bytes.len() > maximum {
        return Err(PayloadCodecError::PayloadTooLarge);
    }
    validate_json_strings(bytes)?;
    serde_json_core::from_slice(bytes)
        .map(|(value, _consumed)| value)
        .map_err(|_| PayloadCodecError::InvalidJson)
}

/// serde-json-core's borrowed string parser accepts raw control bytes and
/// defers escape validation. Check every key and value before typed decoding,
/// including strings whose visitor only borrows the encoded contents.
pub(crate) fn validate_json_strings(bytes: &[u8]) -> Result<(), PayloadCodecError> {
    core::str::from_utf8(bytes).map_err(|_| PayloadCodecError::InvalidJson)?;
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'"' {
            let end = string_end(bytes, offset)?;
            let (value, _) = serde_json_core::from_slice::<EscapedStr<'_>>(&bytes[offset..end])
                .map_err(|_| PayloadCodecError::InvalidJson)?;
            for fragment in fragments(value) {
                fragment?;
            }
            offset = end;
        } else {
            offset += 1;
        }
    }
    Ok(())
}

/// Serde accepts {"variant":null} as well as strings for unit enum variants.
/// For enums that also have data variants, check the named top-level fields
/// after typed decoding has validated the complete object. String-only enums
/// use `deserialize_string_enum` instead.
pub(crate) fn require_top_level_strings(
    payload: &[u8],
    fields: &[&str],
) -> Result<(), PayloadCodecError> {
    let mut depth = 0usize;
    let mut offset = 0;
    while offset < payload.len() {
        match payload[offset] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.checked_sub(1).ok_or(PayloadCodecError::InvalidJson)?,
            b'"' => {
                let start = offset;
                offset = string_end(payload, offset)?;
                let after = skip_space(payload, offset);
                if depth == 1 && payload.get(after) == Some(&b':') {
                    let (key, _) =
                        serde_json_core::from_slice::<EscapedStr<'_>>(&payload[start..offset])
                            .map_err(|_| PayloadCodecError::InvalidJson)?;
                    let value = skip_space(payload, after + 1);
                    for field in fields {
                        if key_matches(&key, field)? && payload.get(value) != Some(&b'"') {
                            return Err(PayloadCodecError::InvalidJson);
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        offset += 1;
    }
    Ok(())
}

fn key_matches(key: &EscapedStr<'_>, field: &str) -> Result<bool, PayloadCodecError> {
    let mut expected = field.chars();
    for fragment in fragments(*key) {
        match fragment.map_err(|_| PayloadCodecError::InvalidJson)? {
            EscapedStringFragment::NotEscaped(text) => {
                for ch in text.chars() {
                    if expected.next() != Some(ch) {
                        return Ok(false);
                    }
                }
            }
            EscapedStringFragment::Escaped(ch) => {
                if expected.next() != Some(ch) {
                    return Ok(false);
                }
            }
        }
    }
    Ok(expected.next().is_none())
}

fn string_end(payload: &[u8], mut offset: usize) -> Result<usize, PayloadCodecError> {
    offset += 1;
    while offset < payload.len() {
        match payload[offset] {
            b'"' => return Ok(offset + 1),
            b'\\' => offset += 1,
            _ => {}
        }
        offset += 1;
    }
    Err(PayloadCodecError::InvalidJson)
}

fn skip_space(payload: &[u8], mut offset: usize) -> usize {
    while payload
        .get(offset)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        offset += 1;
    }
    offset
}

pub(crate) fn deserialize_string_enum<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct EnumVisitor<T>(core::marker::PhantomData<T>);
    impl<T: serde::de::DeserializeOwned> serde::de::Visitor<'_> for EnumVisitor<T> {
        type Value = T;
        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("an enum variant string")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<T, E> {
            T::deserialize(serde::de::value::StrDeserializer::<E>::new(value))
        }
    }
    deserializer.deserialize_str(EnumVisitor(core::marker::PhantomData))
}

#[cfg(test)]
#[path = "../tests/json/codec.rs"]
mod tests;
