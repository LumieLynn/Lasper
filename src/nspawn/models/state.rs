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
        self.state.cmp(&other.state).then(self.name.cmp(&other.name))
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

    pub fn total_rows(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| !g.properties.is_empty())
            .map(|g| g.properties.len() + 2) // 1 header + N props + 1 spacer
            .sum()
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
