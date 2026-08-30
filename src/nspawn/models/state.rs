use serde::{Deserialize, Serialize};

// Compatibility re-exports remain only while the remaining configuration and
// provisioning models are classified. Runtime ownership lives in domain.
#[allow(unused_imports)]
pub use crate::domain::runtime::{
    ImageEntry, ImageName, ImageNameError, ImageVisibility, MachineClass, MachineEntry,
    MachineProvider, MachineRuntimeIdentity, MachineState, RuntimeIdentityError, RuntimeSnapshot,
    StatusUpdate,
};

/// A group of related properties for a machine/container.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PropertyGroup {
    pub name: String,
    pub properties: std::collections::HashMap<String, String>,
}

pub const GROUP_MACHINE: &str = "Machine";
pub const GROUP_SYSTEMD_UNIT: &str = "Systemd";
pub const GROUP_DEPENDENCIES: &str = "Dependencies";

impl PropertyGroup {
    pub fn display_priority(&self) -> u8 {
        match self.name.as_str() {
            GROUP_MACHINE => 0,
            GROUP_SYSTEMD_UNIT => 1,
            GROUP_DEPENDENCIES => 10,
            _ => 5,
        }
    }
}

pub const IMPORTANT_KEYS: &[&str] = &[
    "Name",
    "State",
    "Class",
    "Enabled",
    "IPAddresses",
    "MainPID",
    "Leader",
    "Timestamp",
    "Type",
    "ReadOnly",
    "Usage",
];

/// Strongly-typed properties for a machine/container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionSource {
    #[default]
    Unknown,
    Dbus,
    Cli,
    RuntimeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionCompleteness {
    #[default]
    Unknown,
    Full,
    RuntimeOnly,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MachineProperties {
    #[serde(default)]
    pub source: InspectionSource,
    #[serde(default)]
    pub completeness: InspectionCompleteness,
    /// Grouped properties (e.g., GROUP_MACHINE, GROUP_SYSTEMD_UNIT, GROUP_DEPENDENCIES).
    pub groups: Vec<PropertyGroup>,
    // Placeholders for future metrics
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

    pub fn get_group_mut(&mut self, name: &str) -> &mut std::collections::HashMap<String, String> {
        if let Some(pos) = self.groups.iter().position(|g| g.name == name) {
            &mut self.groups[pos].properties
        } else {
            self.groups.push(PropertyGroup {
                name: name.to_string(),
                properties: std::collections::HashMap::new(),
            });
            &mut self.groups.last_mut().unwrap().properties
        }
    }

    pub fn get_group(&self, name: &str) -> Option<&std::collections::HashMap<String, String>> {
        self.groups
            .iter()
            .find(|group| group.name == name)
            .map(|group| &group.properties)
    }

    pub fn insert(&mut self, group: &str, key: String, value: String) {
        self.get_group_mut(group).insert(key, value);
    }

    /// Returns a filtered and ordered list of 'primary' properties for summary views.
    pub fn get_summary(&self) -> Vec<(&String, &String)> {
        let mut pairs = Vec::new();
        for group in &self.groups {
            for (k, v) in &group.properties {
                if IMPORTANT_KEYS.contains(&k.as_str()) {
                    pairs.push((k, v));
                }
            }
        }

        // Sort by the order defined in IMPORTANT_KEYS
        pairs.sort_by_key(|(k, _)| {
            IMPORTANT_KEYS
                .iter()
                .position(|&ik| ik == k.as_str())
                .unwrap_or(usize::MAX)
        });

        pairs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CpuRepresentation {
    /// Aggregate usage across all cores (e.g., 230% for 2.3 cores)
    Aggregate,
    /// Normalized to total system capacity (e.g., 28% for 230% on an 8-core system)
    Normalized,
}

#[derive(Debug, Clone)]
pub struct MachineMetrics {
    /// Time-series for CPU usage: (timestamp_offset_secs, percentage)
    pub cpu_history: Vec<(f64, f64)>,
    /// Time-series for RAM usage: (timestamp_offset_secs, megabytes)
    pub ram_history: Vec<(f64, f64)>,
}

impl Default for MachineMetrics {
    fn default() -> Self {
        Self {
            cpu_history: Vec::with_capacity(61),
            ram_history: Vec::with_capacity(61),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_state_labels() {
        assert_eq!(MachineState::Running.label(), "running");
        assert_eq!(MachineState::Starting.label(), "starting");
        assert_eq!(MachineState::Exiting.label(), "exiting");
    }

    #[test]
    fn image_names_match_systemd_hidden_image_rule() {
        assert!(ImageEntry::is_protected_name(".host"));
        assert!(!ImageEntry::is_protected_name(".oci-sha256:abc"));
        assert!(ImageEntry::is_hidden_name(".oci-sha256:abc"));
        assert!(ImageEntry::is_hidden_name(".download"));
        assert!(!ImageEntry::is_hidden_name("ubuntu-resolute"));
    }

    #[test]
    fn image_names_follow_systemd_filename_rules() {
        assert!(ImageEntry::is_valid_name(".oci-sha256:a3679419"));
        assert!(ImageEntry::is_valid_name("ubuntu-resolute"));
        assert!(!ImageEntry::is_valid_name("../host"));
        assert!(!ImageEntry::is_valid_name("image/name"));
        assert!(ImageEntry::is_valid_name("Ubuntu Resolute 镜像"));
        assert!(ImageEntry::is_valid_name(&"x".repeat(255)));
        assert!(!ImageEntry::is_valid_name(&"x".repeat(256)));
        assert!(!ImageEntry::is_valid_name(""));
        assert!(!ImageEntry::is_valid_name("."));
        assert!(!ImageEntry::is_valid_name(".."));
        assert!(!ImageEntry::is_valid_name(".#temporary"));
        assert!(!ImageEntry::is_valid_name("name\nwith-control"));
        assert!(!ImageEntry::is_valid_name("name\u{7f}"));
        assert!(ImageEntry::is_valid_name("name\u{85}"));
    }

    #[test]
    fn image_name_deserialization_revalidates_the_filename_component() {
        let hidden: ImageName = serde_json::from_str(r#"".oci-sha256:abc""#).unwrap();
        assert_eq!(hidden.as_str(), ".oci-sha256:abc");
        assert!(serde_json::from_str::<ImageName>(r#""../host""#).is_err());
        assert!(serde_json::from_str::<ImageName>(r#""image/name""#).is_err());
        assert!(serde_json::from_str::<ImageName>(r#"".#temporary""#).is_err());
    }

    #[test]
    fn image_visibility_does_not_infer_image_origin() {
        let image = |name: &str, image_type: &str, readonly: bool| ImageEntry {
            name: name.into(),
            image_type: image_type.into(),
            readonly,
            usage: None,
            dbus_object_path: None,
        };

        assert_eq!(
            image("ubuntu", "directory", false).visibility(),
            ImageVisibility::Regular
        );
        assert_eq!(
            image(".download", "subvolume", true).visibility(),
            ImageVisibility::Hidden
        );
        assert_eq!(
            image(".oci-sha256:abc", "subvolume", true).visibility(),
            ImageVisibility::Hidden
        );
        assert_eq!(
            image(".unrecognized", "mstack", false).visibility(),
            ImageVisibility::Hidden
        );
    }

    #[test]
    fn runtime_snapshot_normalizes_backend_output_order() {
        let machine = |name: &str, addresses: Vec<&str>| MachineEntry {
            name: name.into(),
            class: MachineEntry::NSPAWN_CLASS.into(),
            service: MachineEntry::NSPAWN_SERVICE.into(),
            state: MachineState::Running,
            address: addresses.first().map(|address| (*address).into()),
            all_addresses: addresses.into_iter().map(str::to_string).collect(),
        };
        let image = |name: &str| ImageEntry {
            name: name.into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        };

        let snapshot = RuntimeSnapshot::new(
            vec![
                machine("zeta", vec![]),
                machine("alpha", vec!["fd00::2", "10.0.0.2", "10.0.0.2"]),
            ],
            vec![image("zeta"), image("alpha")],
        );

        assert_eq!(snapshot.machines[0].name, "alpha");
        assert_eq!(snapshot.machines[0].address.as_deref(), Some("10.0.0.2"));
        assert_eq!(snapshot.machines[0].all_addresses, ["10.0.0.2", "fd00::2"]);
        assert_eq!(snapshot.images[0].name, "alpha");
    }

    #[test]
    fn only_stable_running_state_accepts_runtime_actions() {
        assert!(MachineState::Running.accepts_runtime_actions());
        assert!(!MachineState::Starting.accepts_runtime_actions());
        assert!(!MachineState::Exiting.accepts_runtime_actions());
    }

    fn make_entry(name: &str, state: MachineState) -> MachineEntry {
        MachineEntry::optimistic_nspawn(name, state)
    }

    #[test]
    fn machine_entry_ordering() {
        let mut entries = [
            make_entry("z", MachineState::Running),
            make_entry("a", MachineState::Running),
            make_entry("b", MachineState::Exiting),
        ];
        entries.sort();
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[1].name, "b");
        assert_eq!(entries[2].name, "z");
    }

    #[test]
    fn machine_entry_ordering_all_runtime_states() {
        let mut entries = [
            make_entry("c", MachineState::Exiting),
            make_entry("a", MachineState::Running),
            make_entry("b", MachineState::Starting),
        ];
        entries.sort();
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[1].name, "b");
        assert_eq!(entries[2].name, "c");
    }

    #[test]
    fn optimistic_launch_preserves_its_registration_source() {
        let machine = make_entry("test", MachineState::Running);
        assert_eq!(machine.class, "container");
        assert_eq!(machine.service, "systemd-nspawn");
    }

    #[test]
    fn image_entry_ordering_is_name_based() {
        let mut images = [
            ImageEntry {
                name: "z-image".into(),
                image_type: "directory".into(),
                readonly: false,
                usage: None,
                dbus_object_path: None,
            },
            ImageEntry {
                name: "a-image".into(),
                image_type: "subvolume".into(),
                readonly: true,
                usage: None,
                dbus_object_path: None,
            },
        ];
        images.sort();
        assert_eq!(images[0].name, "a-image");
        assert_eq!(images[1].name, "z-image");
    }

    #[test]
    fn machine_entry_ordering_empty() {
        let mut entries: Vec<MachineEntry> = vec![];
        entries.sort();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_property_group_priority() {
        assert_eq!(
            PropertyGroup {
                name: GROUP_MACHINE.into(),
                properties: Default::default()
            }
            .display_priority(),
            0
        );
        assert_eq!(
            PropertyGroup {
                name: GROUP_SYSTEMD_UNIT.into(),
                properties: Default::default()
            }
            .display_priority(),
            1
        );
        assert_eq!(
            PropertyGroup {
                name: GROUP_DEPENDENCIES.into(),
                properties: Default::default()
            }
            .display_priority(),
            10
        );
        assert_eq!(
            PropertyGroup {
                name: "Other".into(),
                properties: Default::default()
            }
            .display_priority(),
            5
        );
        assert_eq!(
            PropertyGroup {
                name: "SomethingNew".into(),
                properties: Default::default()
            }
            .display_priority(),
            5
        );
    }

    #[test]
    fn test_machine_properties_summary() {
        let mut props = MachineProperties::default();
        props.insert(GROUP_MACHINE, "Name".to_string(), "test".to_string());
        props.insert(GROUP_MACHINE, "State".to_string(), "running".to_string());
        props.insert(GROUP_MACHINE, "Unknown".to_string(), "val".to_string());

        let summary = props.get_summary();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].0, "Name");
        assert_eq!(summary[1].0, "State");
    }

    #[test]
    fn test_machine_properties_summary_no_important_keys() {
        let mut props = MachineProperties::default();
        props.insert(GROUP_MACHINE, "SomeRandom".to_string(), "val".to_string());
        props.insert(
            GROUP_MACHINE,
            "AnotherRandom".to_string(),
            "val2".to_string(),
        );

        let summary = props.get_summary();
        assert!(summary.is_empty());
    }

    #[test]
    fn test_get_group_mut_creates_once() {
        let mut props = MachineProperties::default();
        props.get_group_mut(GROUP_MACHINE);
        props.get_group_mut(GROUP_MACHINE);
        // Should only create the group once
        assert_eq!(props.groups.len(), 1);
    }
}
