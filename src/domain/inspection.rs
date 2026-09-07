//! Lossless read models for the common systemd-machined inspection plane.
//!
//! Adapters populate these snapshots from D-Bus, systemd tools output, or runtime state.
//! Unknown property groups and keys are retained so a provider-specific value
//! is never silently discarded at the application boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A group of related properties reported by a runtime inspector.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PropertyGroup {
    pub name: String,
    pub properties: HashMap<String, String>,
}

/// Well-known group names used by the current inspectors.
pub const GROUP_MACHINE: &str = "Machine";
pub const GROUP_SYSTEMD_UNIT: &str = "Systemd";
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

    pub fn get_group_mut(&mut self, name: &str) -> &mut HashMap<String, String> {
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

    pub fn get_group(&self, name: &str) -> Option<&HashMap<String, String>> {
        self.groups
            .iter()
            .find(|group| group.name == name)
            .map(|group| &group.properties)
    }

    pub fn insert(&mut self, group: &str, key: String, value: String) {
        self.get_group_mut(group).insert(key, value);
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
            properties.get_group("ProviderSpecific").unwrap()["OpaqueKey"],
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
