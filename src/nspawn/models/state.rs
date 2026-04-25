#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContainerState {
    Running,
    Starting,
    Exiting,
    Off,
}

impl ContainerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Off => "poweroff",
            Self::Starting => "starting",
            Self::Exiting => "exiting",
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running | Self::Starting | Self::Exiting)
    }
}

/// A container known to machinectl — either running, poweroff, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEntry {
    /// The name used by machinectl
    pub name: String,
    /// Current lifecycle state
    pub state: ContainerState,
    /// Image type ("directory", "raw", "tar", …) — from list-images, None if only seen running
    pub image_type: Option<String>,
    /// Whether the image is read-only (from list-images)
    pub readonly: bool,
    /// Disk usage string (from list-images)
    pub usage: Option<String>,
    /// Network address (from list, only when running)
    pub address: Option<String>,
    /// All network addresses
    pub all_addresses: Vec<String>,
}

impl Ord for ContainerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.state
            .cmp(&other.state)
            .then(self.name.cmp(&other.name))
    }
}

impl PartialOrd for ContainerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A group of related properties for a machine/container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PropertyGroup {
    pub name: String,
    pub properties: std::collections::HashMap<String, String>,
}

impl PropertyGroup {
    pub fn display_priority(&self) -> u8 {
        match self.name.as_str() {
            "Machine" => 0,
            "Systemd Unit" => 1,
            "Dependencies" => 10,
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
#[derive(Debug, Clone, Default)]
pub struct MachineProperties {
    /// Grouped properties (e.g., "Machine", "Systemd Unit", "Dependencies").
    pub groups: Vec<PropertyGroup>,
    // Placeholders for future metrics
    #[allow(dead_code)]
    pub cpu_usage: Option<f64>,
    #[allow(dead_code)]
    pub memory_usage: Option<u64>,
}

impl MachineProperties {
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
pub struct ContainerMetrics {
    /// Time-series for CPU usage: (timestamp_offset_secs, percentage)
    pub cpu_history: Vec<(f64, f64)>,
    /// Time-series for RAM usage: (timestamp_offset_secs, megabytes)
    pub ram_history: Vec<(f64, f64)>,
}

impl Default for ContainerMetrics {
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
    fn test_container_state_labels() {
        assert_eq!(ContainerState::Running.label(), "running");
        assert_eq!(ContainerState::Off.label(), "poweroff");
        assert_eq!(ContainerState::Starting.label(), "starting");
        assert_eq!(ContainerState::Exiting.label(), "exiting");
    }

    #[test]
    fn test_container_state_is_running() {
        assert!(ContainerState::Running.is_running());
        assert!(ContainerState::Starting.is_running());
        assert!(ContainerState::Exiting.is_running());
        assert!(!ContainerState::Off.is_running());
    }

    fn make_entry(name: &str, state: ContainerState) -> ContainerEntry {
        ContainerEntry {
            name: name.to_string(),
            state,
            image_type: None,
            readonly: false,
            usage: None,
            address: None,
            all_addresses: vec![],
        }
    }

    #[test]
    fn test_container_entry_ordering() {
        let mut entries = vec![
            make_entry("z", ContainerState::Running),
            make_entry("a", ContainerState::Running),
            make_entry("b", ContainerState::Off),
        ];
        entries.sort();
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[1].name, "z");
        assert_eq!(entries[2].name, "b");
    }

    #[test]
    fn test_container_entry_ordering_all_states() {
        let mut entries = vec![
            make_entry("d", ContainerState::Off),
            make_entry("c", ContainerState::Exiting),
            make_entry("a", ContainerState::Running),
            make_entry("b", ContainerState::Starting),
        ];
        entries.sort();
        // Ord is derived enum order: Running(0) < Starting(1) < Exiting(2) < Off(3)
        assert_eq!(entries[0].name, "a"); // Running
        assert_eq!(entries[1].name, "b"); // Starting
        assert_eq!(entries[2].name, "c"); // Exiting
        assert_eq!(entries[3].name, "d"); // Off
    }

    #[test]
    fn test_container_entry_ordering_empty() {
        let mut entries: Vec<ContainerEntry> = vec![];
        entries.sort();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_property_group_priority() {
        assert_eq!(
            PropertyGroup {
                name: "Machine".into(),
                properties: Default::default()
            }
            .display_priority(),
            0
        );
        assert_eq!(
            PropertyGroup {
                name: "Systemd Unit".into(),
                properties: Default::default()
            }
            .display_priority(),
            1
        );
        assert_eq!(
            PropertyGroup {
                name: "Dependencies".into(),
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
        props.insert("Machine", "Name".to_string(), "test".to_string());
        props.insert("Machine", "State".to_string(), "running".to_string());
        props.insert("Machine", "Unknown".to_string(), "val".to_string());

        let summary = props.get_summary();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].0, "Name");
        assert_eq!(summary[1].0, "State");
    }

    #[test]
    fn test_machine_properties_summary_no_important_keys() {
        let mut props = MachineProperties::default();
        props.insert("Machine", "SomeRandom".to_string(), "val".to_string());
        props.insert("Machine", "AnotherRandom".to_string(), "val2".to_string());

        let summary = props.get_summary();
        assert!(summary.is_empty());
    }

    #[test]
    fn test_get_group_mut_creates_once() {
        let mut props = MachineProperties::default();
        props.get_group_mut("Machine");
        props.get_group_mut("Machine");
        // Should only create the group once
        assert_eq!(props.groups.len(), 1);
    }
}
