//! Generic typed property printing, corresponding to bus-print-properties.c.

use crate::domain::inspection::PropertyValue;

pub(super) fn generic(value: &PropertyValue) -> String {
    match value {
        PropertyValue::CanonicalText(text) => text.clone(),
        PropertyValue::String(text) => {
            if text.contains('\n') {
                "[unprintable]".into()
            } else {
                text.clone()
            }
        }
        PropertyValue::Bool(true) => "yes".into(),
        PropertyValue::Bool(false) => "no".into(),
        PropertyValue::U8(value) => value.to_string(),
        PropertyValue::I16(value) => value.to_string(),
        PropertyValue::U16(value) => value.to_string(),
        PropertyValue::I32(value) => value.to_string(),
        PropertyValue::U32(value) => value.to_string(),
        PropertyValue::I64(value) => value.to_string(),
        PropertyValue::U64(value) => value.to_string(),
        PropertyValue::F64(value) => value.to_string(),
        PropertyValue::ObjectPath(value) | PropertyValue::Signature(value) => value.clone(),
        PropertyValue::Array(array) => match array.element_signature() {
            "s" => format_string_array(value),
            "y" => array
                .iter()
                .map(|value| match value {
                    PropertyValue::U8(value) => format!("{value:02x}"),
                    _ => generic(value),
                })
                .collect(),
            "u" => array
                .iter()
                .map(|value| match value {
                    PropertyValue::U32(value) => format!("{value:08x}"),
                    _ => generic(value),
                })
                .collect(),
            _ => array.iter().map(generic).collect::<Vec<_>>().join(" "),
        },
        PropertyValue::Structure(fields) => format!(
            "({})",
            fields.iter().map(generic).collect::<Vec<_>>().join(", ")
        ),
        PropertyValue::Dictionary { entries, .. } => entries
            .iter()
            .map(|(key, value)| format!("{}={}", generic(key), generic(value)))
            .collect::<Vec<_>>()
            .join(", "),
        PropertyValue::Variant(value) => generic(value),
        PropertyValue::FileDescriptor => "[file descriptor]".into(),
    }
}

/// `bus-print-properties` only has a generic representation for primitive
/// values and the three simple arrays it knows how to print. Structured
/// values are rendered by a property-specific rule; anything else is the
/// same opaque value that systemctl reports as `[unprintable]`.
pub(super) fn is_supported(value: &PropertyValue) -> bool {
    match value {
        PropertyValue::CanonicalText(_)
        | PropertyValue::String(_)
        | PropertyValue::Bool(_)
        | PropertyValue::I32(_)
        | PropertyValue::U32(_)
        | PropertyValue::I64(_)
        | PropertyValue::U64(_)
        | PropertyValue::F64(_) => true,
        PropertyValue::U8(_) | PropertyValue::I16(_) | PropertyValue::U16(_) => false,
        PropertyValue::ObjectPath(_) | PropertyValue::Signature(_) => false,
        PropertyValue::Array(array) => matches!(array.element_signature(), "s" | "y" | "u"),
        PropertyValue::Structure(_)
        | PropertyValue::Dictionary { .. }
        | PropertyValue::Variant(_)
        | PropertyValue::FileDescriptor => false,
    }
}

pub(super) fn format_fields(fields: &[PropertyValue]) -> String {
    fields.iter().map(generic).collect::<Vec<_>>().join(" ")
}

pub(super) fn format_string_array(value: &PropertyValue) -> String {
    let PropertyValue::Array(array) = value else {
        return generic(value);
    };
    array
        .iter()
        .map(|value| match value {
            PropertyValue::String(text) => shell_maybe_quote(text),
            _ => generic(value),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn format_yes_no(value: &PropertyValue) -> &'static str {
    if matches!(value, PropertyValue::Bool(true)) {
        "yes"
    } else {
        "no"
    }
}

fn shell_maybe_quote(value: &str) -> String {
    // systemd's non-POSIX SHELL_ESCAPE_EMPTY representation. Non-ASCII text
    // is already valid UTF-8; it does not itself require quotes.
    if !value.is_empty()
        && !value.chars().any(|character| {
            character.is_ascii_control()
                || character.is_ascii_whitespace()
                || "\"\\'`$*?[()<>|&;!".contains(character)
        })
    {
        return value.into();
    }
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' | '"' | '`' | '$' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{7}' => quoted.push_str("\\a"),
            '\u{8}' => quoted.push_str("\\b"),
            '\u{c}' => quoted.push_str("\\f"),
            '\u{b}' => quoted.push_str("\\v"),
            character if character.is_ascii_control() => {
                quoted.push_str(&format!("\\{:03o}", character as u32));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}
