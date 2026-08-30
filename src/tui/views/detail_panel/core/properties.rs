use crate::domain::inspection::{
    MachineProperties, PropertyGroup, GROUP_DEPENDENCIES, GROUP_MACHINE, GROUP_SYSTEMD_UNIT,
};

const IMPORTANT_KEYS: &[&str] = &[
    "Name",
    "State",
    "Class",
    "Service",
    "Enabled",
    "IPAddresses",
    "MainPID",
    "Leader",
    "Timestamp",
    "Type",
    "ReadOnly",
    "Usage",
];

pub(crate) fn summary_properties(properties: &MachineProperties) -> Vec<(&String, &String)> {
    let mut pairs = properties
        .groups
        .iter()
        .flat_map(|group| group.properties.iter())
        .filter(|(key, _)| IMPORTANT_KEYS.contains(&key.as_str()))
        .collect::<Vec<_>>();
    pairs.sort_by_key(|(key, _)| {
        IMPORTANT_KEYS
            .iter()
            .position(|important| important == &key.as_str())
            .unwrap_or(usize::MAX)
    });
    pairs
}

pub(crate) fn group_display_priority(group: &PropertyGroup) -> u8 {
    match group.name.as_str() {
        GROUP_MACHINE => 0,
        GROUP_SYSTEMD_UNIT => 1,
        GROUP_DEPENDENCIES => 10,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_keeps_only_important_keys_in_stable_order() {
        let mut properties = MachineProperties::default();
        properties.insert(GROUP_MACHINE, "Unknown".into(), "ignored".into());
        properties.insert(GROUP_MACHINE, "State".into(), "running".into());
        properties.insert(GROUP_MACHINE, "Name".into(), "machine".into());

        let summary = summary_properties(&properties);
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].0.as_str(), "Name");
        assert_eq!(summary[1].0.as_str(), "State");
    }

    #[test]
    fn unknown_group_priority_stays_between_core_and_dependencies() {
        assert_eq!(
            group_display_priority(&PropertyGroup {
                name: GROUP_MACHINE.into(),
                properties: Default::default(),
            }),
            0
        );
        assert_eq!(
            group_display_priority(&PropertyGroup {
                name: "ProviderSpecific".into(),
                properties: Default::default(),
            }),
            5
        );
        assert_eq!(
            group_display_priority(&PropertyGroup {
                name: GROUP_DEPENDENCIES.into(),
                properties: Default::default(),
            }),
            10
        );
    }
}
