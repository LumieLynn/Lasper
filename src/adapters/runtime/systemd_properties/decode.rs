//! Decode typed D-Bus values or canonical systemd tools output. The domain
//! model stays independent of zbus and the elevated transport preserves it.

use crate::domain::inspection::{PropertyArray, PropertyValue};
use std::collections::HashMap;
use zbus::zvariant::Value;

pub(super) fn decode(value: &Value<'_>) -> PropertyValue {
    match value {
        Value::Bool(value) => PropertyValue::Bool(*value),
        Value::U8(value) => PropertyValue::U8(*value),
        Value::U16(value) => PropertyValue::U16(*value),
        Value::U32(value) => PropertyValue::U32(*value),
        Value::U64(value) => PropertyValue::U64(*value),
        Value::I16(value) => PropertyValue::I16(*value),
        Value::I32(value) => PropertyValue::I32(*value),
        Value::I64(value) => PropertyValue::I64(*value),
        Value::F64(value) => PropertyValue::F64(*value),
        Value::Str(value) => PropertyValue::String(value.as_str().to_string()),
        Value::ObjectPath(value) => PropertyValue::ObjectPath(value.as_str().to_string()),
        Value::Signature(value) => PropertyValue::Signature(value.to_string()),
        Value::Array(values) => PropertyValue::Array(PropertyArray {
            element_signature: values.element_signature().to_string(),
            values: values.iter().map(decode).collect(),
        }),
        Value::Dict(values) => PropertyValue::Dictionary {
            signature: values.signature().to_string(),
            entries: values
                .iter()
                .map(|(key, value)| (decode(key), decode(value)))
                .collect(),
        },
        Value::Structure(value) => {
            PropertyValue::Structure(value.fields().iter().map(decode).collect())
        }
        Value::Value(value) => PropertyValue::Variant(Box::new(decode(value))),
        // Query inspection never transfers descriptor ownership as a value.
        // Retain the wire type without leaking a meaningless process-local fd.
        Value::Fd(_) => PropertyValue::FileDescriptor,
    }
}

/// `show` emits some array properties (DeviceAllow, ExecStart, etc.) on
/// repeated lines. Keep every member in output order instead of overwriting
/// earlier values, while preserving each member's canonical text.
pub(crate) fn tool_properties(output: &[u8]) -> Vec<(String, String)> {
    let mut properties: HashMap<String, String> = HashMap::new();
    for (name, value) in String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
    {
        properties
            .entry(name.trim().into())
            .and_modify(|text| {
                text.push(' ');
                text.push_str(value);
            })
            .or_insert_with(|| value.into());
    }
    properties.into_iter().collect()
}
