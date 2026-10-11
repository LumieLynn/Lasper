//! Typed read models for the common systemd-machined inspection plane.
//!
//! Adapters populate these snapshots from D-Bus, systemd tools output, or runtime state.
//! Unknown property groups and keys are retained so a provider-specific value
//! is never silently discarded at the application boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A backend-neutral representation of a property value.
///
/// D-Bus properties must keep their type until the inspection renderer has
/// applied the small set of systemd-specific semantic rules.  Tool-backed
/// inspections use [`PropertyValue::CanonicalText`] because `systemctl show` has already performed
/// that rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PropertyValue {
    CanonicalText(String),
    String(String),
    Bool(bool),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    ObjectPath(String),
    Signature(String),
    Array(PropertyArray),
    Structure(Vec<PropertyValue>),
    Dictionary {
        signature: String,
        entries: Vec<(PropertyValue, PropertyValue)>,
    },
    Variant(Box<PropertyValue>),
    FileDescriptor,
}

impl PropertyValue {
    pub fn text(value: impl Into<String>) -> Self {
        Self::CanonicalText(value.into())
    }

    pub fn signature(&self) -> String {
        match self {
            Self::CanonicalText(_) => String::new(),
            Self::String(_) => "s".into(),
            Self::Bool(_) => "b".into(),
            Self::U8(_) => "y".into(),
            Self::I16(_) => "n".into(),
            Self::U16(_) => "q".into(),
            Self::I32(_) => "i".into(),
            Self::U32(_) => "u".into(),
            Self::I64(_) => "x".into(),
            Self::U64(_) => "t".into(),
            Self::F64(_) => "d".into(),
            Self::ObjectPath(_) => "o".into(),
            Self::Signature(_) => "g".into(),
            Self::Array(array) => format!("a{}", array.element_signature),
            Self::Structure(fields) => format!(
                "({})",
                fields.iter().map(Self::signature).collect::<String>()
            ),
            Self::Dictionary { signature, .. } => signature.clone(),
            Self::Variant(_) => "v".into(),
            Self::FileDescriptor => "h".into(),
        }
    }
}

/// Array element signatures must survive even when the array is empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyArray {
    pub element_signature: String,
    pub values: Vec<PropertyValue>,
}

impl PropertyArray {
    pub fn iter(&self) -> std::slice::Iter<'_, PropertyValue> {
        self.values.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn element_signature(&self) -> &str {
        &self.element_signature
    }
}

/// One inspection property and its human-readable display value.
///
/// Keeping both forms lets lifecycle code consume stable text while future
/// consumers can still inspect the original type without reparsing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyEntry {
    pub value: PropertyValue,
    pub display: String,
    /// The actual D-Bus interface, when provided by the typed backend.
    /// Tool output is flat and does not claim an observed interface.
    pub interface: Option<String>,
}

impl PropertyEntry {
    pub fn text(display: impl Into<String>) -> Self {
        let display = display.into();
        Self {
            value: PropertyValue::text(display.clone()),
            display,
            interface: None,
        }
    }

    pub fn typed(value: PropertyValue, display: impl Into<String>) -> Self {
        Self {
            value,
            display: display.into(),
            interface: None,
        }
    }
}

/// A group of related properties reported by a runtime inspector.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PropertyGroup {
    pub name: String,
    pub properties: HashMap<String, PropertyEntry>,
}

/// Well-known group names used by the current inspectors.
pub const GROUP_MACHINE: &str = "Machine";
pub const GROUP_SYSTEMD_UNIT: &str = "Systemd Unit";
pub const GROUP_SYSTEMD_SERVICE: &str = "Systemd Service";
pub const GROUP_DEPENDENCIES: &str = "Dependencies";

/// Where an inspection snapshot came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionSource {
    #[default]
    Unknown,
    Dbus,
    SystemdTools,
    RuntimeState,
}

/// How complete an inspection snapshot is for its target/provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionCompleteness {
    #[default]
    Unknown,
    Full,
    RuntimeOnly,
}

/// Extensible properties returned by the common machined inspection plane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MachineProperties {
    #[serde(default)]
    pub source: InspectionSource,
    #[serde(default)]
    pub completeness: InspectionCompleteness,
    pub groups: Vec<PropertyGroup>,
    /// Reserved for a future typed metrics snapshot; not populated by the
    /// current inspectors.
    #[allow(dead_code)]
    pub cpu_usage: Option<f64>,
    #[allow(dead_code)]
    pub memory_usage: Option<u64>,
}

impl MachineProperties {
    pub fn from_inspection(source: InspectionSource, completeness: InspectionCompleteness) -> Self {
        Self {
            source,
            completeness,
            ..Self::default()
        }
    }

    pub fn get_group_mut(&mut self, name: &str) -> &mut HashMap<String, PropertyEntry> {
        if let Some(pos) = self.groups.iter().position(|group| group.name == name) {
            &mut self.groups[pos].properties
        } else {
            let index = self.groups.len();
            self.groups.push(PropertyGroup {
                name: name.to_string(),
                properties: HashMap::new(),
            });
            &mut self.groups[index].properties
        }
    }

    pub fn get_group(&self, name: &str) -> Option<&HashMap<String, PropertyEntry>> {
        self.groups
            .iter()
            .find(|group| group.name == name)
            .map(|group| &group.properties)
    }

    pub fn insert(&mut self, group: &str, key: String, value: String) {
        self.get_group_mut(group)
            .insert(key, PropertyEntry::text(value));
    }

    pub fn get_display(&self, group: &str, key: &str) -> Option<&str> {
        self.get_group(group)
            .and_then(|properties| properties.get(key))
            .map(|entry| entry.display.as_str())
    }

    pub fn systemd_display(&self, key: &str) -> Option<&str> {
        self.get_display(GROUP_SYSTEMD_UNIT, key)
            .or_else(|| self.get_display(GROUP_SYSTEMD_SERVICE, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserting_properties_preserves_unknown_groups_and_keys() {
        let mut properties = MachineProperties::from_inspection(
            InspectionSource::RuntimeState,
            InspectionCompleteness::RuntimeOnly,
        );
        properties.insert(
            "ProviderSpecific",
            "OpaqueKey".into(),
            "opaque-value".into(),
        );
        properties.insert("ProviderSpecific", "OtherKey".into(), "other-value".into());

        assert_eq!(
            properties.get_group("ProviderSpecific").unwrap()["OpaqueKey"].display,
            "opaque-value"
        );
        assert_eq!(properties.groups.len(), 1);
    }

    #[test]
    fn inspection_metadata_round_trips() {
        let properties = MachineProperties::from_inspection(
            InspectionSource::Dbus,
            InspectionCompleteness::Full,
        );
        let json = serde_json::to_value(&properties).unwrap();
        let decoded: MachineProperties = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.source, InspectionSource::Dbus);
        assert_eq!(decoded.completeness, InspectionCompleteness::Full);
    }
}
