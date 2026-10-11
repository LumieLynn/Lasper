//! Machine overview assembled from the current catalog and scoped inspection.

use std::{borrow::Cow, time::Duration};

use ratatui::text::Line;

use crate::domain::inspection::{
    MachineProperties, PropertyEntry, PropertyValue, GROUP_MACHINE, GROUP_SYSTEMD_SERVICE,
    GROUP_SYSTEMD_UNIT,
};
use crate::domain::runtime::MachineEntry;
use crate::tui::features::workspace::inspector::presentation::command_summary;
use crate::tui::features::workspace::inspector::presentation::empty_lines;
use crate::tui::features::workspace::inspector::presentation::layout::error_lines;
use crate::tui::features::workspace::inspector::presentation::property_document::{
    section_lines, PropertyField, PropertySection,
};
use crate::tui::features::workspace::inspector::DetailRenderContext;

pub(crate) fn lines(data: &DetailRenderContext<'_>, width: usize) -> Vec<Line<'static>> {
    data.machine
        .map(|machine| machine_lines(machine, data.properties, width))
        .unwrap_or_else(empty_lines)
}

fn machine_lines(
    machine: &MachineEntry,
    properties: Result<&MachineProperties, &str>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut identity = vec![
        PropertyField::new("Name", machine.name.as_str()),
        PropertyField::property("Status", "State", machine.state.label()),
        PropertyField::new("Class", machine.class.as_str()),
        PropertyField::new("Provider", machine.service.as_str()),
        PropertyField::new("IP addresses", machine.addresses.diagnostic_value()),
    ];

    let mut service = Vec::new();
    let mut resources = Vec::new();
    if let Ok(properties) = properties {
        for (key, label) in [
            ("RootDirectory", "Root directory"),
            ("Leader", "Leader PID"),
            ("Timestamp", "Registered"),
            ("Unit", "Registration unit"),
        ] {
            identity.extend(property_field(properties, GROUP_MACHINE, key, label));
        }

        for (group, key, label) in [
            (GROUP_SYSTEMD_UNIT, "Id", "Unit"),
            (GROUP_SYSTEMD_UNIT, "Description", "Description"),
            (GROUP_SYSTEMD_UNIT, "FragmentPath", "Unit file"),
            (GROUP_SYSTEMD_UNIT, "DropInPaths", "Drop-in paths"),
        ] {
            service.extend(property_field(properties, group, key, label));
        }
        service.extend(
            property_field(
                properties,
                GROUP_SYSTEMD_UNIT,
                "UnitFileState",
                "Start at boot",
            )
            .filter(|field| !field.is_empty())
            .or_else(|| property_field(properties, GROUP_SYSTEMD_UNIT, "Enabled", "Start at boot")),
        );
        for (key, label) in [
            ("Type", "Service type"),
            ("MainPID", "Main PID"),
            ("ControlGroup", "Control group"),
        ] {
            service.extend(property_field(
                properties,
                GROUP_SYSTEMD_SERVICE,
                key,
                label,
            ));
        }
        if let Some(entry) = properties
            .get_group(GROUP_SYSTEMD_SERVICE)
            .and_then(|group| {
                group
                    .get("ExecStartEx")
                    .filter(|entry| !entry.display.is_empty())
                    .or_else(|| group.get("ExecStart"))
            })
        {
            service.push(PropertyField::property(
                "Start command",
                "ExecStart",
                command_summary(entry),
            ));
        }
        // Healthy state is already conveyed by the machine status. Preserve
        // unexpected unit/service state here rather than hiding diagnostics.
        for (group, key, label, normal) in [
            (GROUP_SYSTEMD_UNIT, "LoadState", "Load state", "loaded"),
            (GROUP_SYSTEMD_UNIT, "ActiveState", "Active state", "active"),
            (GROUP_SYSTEMD_UNIT, "SubState", "Substate", "running"),
            (GROUP_SYSTEMD_SERVICE, "Result", "Result", "success"),
        ] {
            if let Some(value) = properties
                .get_display(group, key)
                .filter(|value| *value != normal)
            {
                service.push(PropertyField::property(label, key, value));
            }
        }

        for (key, label, format) in [
            ("MemoryCurrent", "Memory used", ResourceFormat::Bytes),
            ("MemoryPeak", "Memory peak", ResourceFormat::Bytes),
        ] {
            resources.extend(resource_field(properties, key, label, format));
        }
        resources.extend(
            resource_field(
                properties,
                "EffectiveMemoryMax",
                "Memory limit",
                ResourceFormat::Bytes,
            )
            .filter(|field| !field.is_empty())
            .or_else(|| {
                resource_field(
                    properties,
                    "MemoryMax",
                    "Memory limit",
                    ResourceFormat::Bytes,
                )
            }),
        );
        for (key, label, format) in [
            ("TasksCurrent", "Tasks", ResourceFormat::Count),
            ("TasksMax", "Task limit", ResourceFormat::Count),
            ("CPUUsageNSec", "CPU time", ResourceFormat::Nanoseconds),
        ] {
            resources.extend(resource_field(properties, key, label, format));
        }
    }

    let mut lines = section_lines(
        &[
            PropertySection {
                title: "Machine",
                fields: identity,
            },
            PropertySection {
                title: "Service",
                fields: service,
            },
            PropertySection {
                title: "Resources",
                fields: resources,
            },
        ],
        width,
    );
    if let Err(error) = properties {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(error_lines(
            &format!("Additional properties unavailable: {error}"),
            width,
        ));
    }
    lines
}

fn property_field<'a>(
    properties: &'a MachineProperties,
    group: &str,
    key: &'static str,
    label: &'static str,
) -> Option<PropertyField<'a>> {
    properties
        .get_display(group, key)
        .map(|value| PropertyField::property(label, key, value))
}

#[derive(Clone, Copy)]
enum ResourceFormat {
    Bytes,
    Count,
    Nanoseconds,
}

fn resource_field<'a>(
    properties: &'a MachineProperties,
    key: &'static str,
    label: &'static str,
    format: ResourceFormat,
) -> Option<PropertyField<'a>> {
    let entry = properties.get_group(GROUP_SYSTEMD_SERVICE)?.get(key)?;
    Some(PropertyField::property(
        label,
        key,
        resource_value(entry, format),
    ))
}

/// Canonical tool text is already rendered by systemd. Interpret only numeric
/// resource values here; sentinel and unknown text retain their meaning.
fn resource_value(entry: &PropertyEntry, format: ResourceFormat) -> Cow<'_, str> {
    let number = match &entry.value {
        PropertyValue::U64(number) => Some(*number),
        PropertyValue::CanonicalText(text) => text.parse::<u64>().ok(),
        _ => None,
    };
    match number.filter(|number| *number != u64::MAX) {
        Some(number) => match format {
            ResourceFormat::Bytes => Cow::Owned(memory_size(number)),
            ResourceFormat::Nanoseconds => {
                Cow::Owned(format!("{:?}", Duration::from_nanos(number)))
            }
            ResourceFormat::Count => Cow::Owned(number.to_string()),
        },
        None => Cow::Borrowed(&entry.display),
    }
}

fn memory_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{MachineAddressObservation, MachineState};
    use crate::tui::foundation::theme::{init_theme, theme, Theme};
    use ratatui::{buffer::Buffer, widgets::Widget};
    use ratatui::{layout::Rect, widgets::Paragraph};

    fn machine() -> MachineEntry {
        MachineEntry {
            name: "archlinux".into(),
            class: MachineEntry::NSPAWN_CLASS.into(),
            service: MachineEntry::NSPAWN_SERVICE.into(),
            state: MachineState::Starting,
            addresses: MachineAddressObservation::available(["192.0.2.10".into()]),
        }
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn catalog_identity_and_scoped_properties_are_not_overwritten_by_other_groups() {
        init_theme(Theme::dark());
        let mut properties = MachineProperties::default();
        properties.insert(GROUP_MACHINE, "Name".into(), "old-name".into());
        properties.insert(GROUP_MACHINE, "State".into(), "running".into());
        properties.insert(GROUP_MACHINE, "Type".into(), "wrong-type".into());
        properties.insert(
            GROUP_MACHINE,
            "RootDirectory".into(),
            "/var/lib/machines/archlinux".into(),
        );
        properties.insert(
            GROUP_SYSTEMD_SERVICE,
            "RootDirectory".into(),
            "/wrong-root".into(),
        );
        properties.insert(GROUP_SYSTEMD_SERVICE, "Type".into(), "notify".into());
        properties.insert(
            GROUP_SYSTEMD_SERVICE,
            "UnitFileState".into(),
            "wrong-state".into(),
        );
        properties.insert(GROUP_SYSTEMD_UNIT, "UnitFileState".into(), "enabled".into());
        properties.insert(GROUP_SYSTEMD_UNIT, "ActiveState".into(), "failed".into());
        let lines = machine_lines(&machine(), Ok(&properties), 80);
        let rows = text(&lines);
        for expected in [
            "Name: archlinux",
            "Status: starting",
            "IP addresses: 192.0.2.10",
            "Root directory: /var/lib/machines/archlinux",
            "Service type: notify",
            "Start at boot: enabled",
            "Active state: failed",
        ] {
            assert!(
                rows.iter().any(|line| line == expected),
                "missing {expected}"
            );
        }
        assert!(!rows.join("\n").contains("wrong-"));
        assert!(!rows.join("\n").contains("old-name"));
        let failed = lines
            .iter()
            .find(|line| line.to_string() == "Active state: failed")
            .unwrap();
        assert_eq!(failed.spans.last().unwrap().style.fg, Some(theme().error));

        properties.insert(GROUP_SYSTEMD_UNIT, "UnitFileState".into(), String::new());
        properties.insert(
            GROUP_SYSTEMD_UNIT,
            "Enabled".into(),
            "enabled-runtime".into(),
        );
        assert!(text(&machine_lines(&machine(), Ok(&properties), 80))
            .iter()
            .any(|line| line == "Start at boot: enabled-runtime"));
    }

    #[test]
    fn typed_and_tool_resource_values_have_the_same_units_and_keep_sentinels() {
        init_theme(Theme::dark());
        let mut typed = MachineProperties::default();
        for (key, value, display) in [
            ("MemoryCurrent", 128 * 1024 * 1024, "134217728"),
            ("MemoryPeak", u64::MAX, "[not set]"),
            ("MemoryMax", u64::MAX, "infinity"),
            ("TasksCurrent", 27, "27"),
            ("CPUUsageNSec", 1_250_000_000, "1250000000"),
        ] {
            typed.get_group_mut(GROUP_SYSTEMD_SERVICE).insert(
                key.into(),
                PropertyEntry::typed(PropertyValue::U64(value), display),
            );
        }
        typed.insert(
            GROUP_SYSTEMD_SERVICE,
            "EffectiveMemoryMax".into(),
            String::new(),
        );
        let mut tools = MachineProperties::default();
        for (key, entry) in typed.get_group(GROUP_SYSTEMD_SERVICE).unwrap() {
            tools.insert(GROUP_SYSTEMD_SERVICE, key.clone(), entry.display.clone());
        }
        let rows = text(&machine_lines(&machine(), Ok(&typed), 80));
        assert_eq!(rows, text(&machine_lines(&machine(), Ok(&tools), 80)));
        for expected in [
            "Memory used: 128.0 MiB",
            "Memory peak: [not set]",
            "Memory limit: infinity",
            "Tasks: 27",
            "CPU time: 1.25s",
        ] {
            assert!(
                rows.iter().any(|line| line == expected),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn service_overview_prefers_configuration_and_only_reports_unhealthy_states() {
        init_theme(Theme::dark());
        let mut properties = MachineProperties::default();
        for (key, value) in [
            ("Id", "systemd-nspawn@archlinux.service"),
            ("LoadState", "loaded"),
            ("ActiveState", "active"),
            ("SubState", "running"),
            ("ActiveEnterTimestamp", "redundant-time"),
            (
                "FragmentPath",
                "/usr/lib/systemd/system/systemd-nspawn@.service",
            ),
            (
                "DropInPaths",
                "/etc/systemd/system/systemd-nspawn@archlinux.service.d/90-lasper.conf",
            ),
        ] {
            properties.insert(GROUP_SYSTEMD_UNIT, key.into(), value.into());
        }
        for (key, value) in [
            ("Result", "success"),
            ("DeviceAllow", "/dev/dri/renderD128 rw"),
            ("ExecStart", "{ path=/usr/bin/systemd-nspawn ; argv[]=/usr/bin/systemd-nspawn --boot ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=42 ; code=exited ; status=0 }"),
        ] {
            properties.insert(GROUP_SYSTEMD_SERVICE, key.into(), value.into());
        }
        let rows = text(&machine_lines(&machine(), Ok(&properties), 120));
        for expected in [
            "Unit file: /usr/lib/systemd/system/systemd-nspawn@.service",
            "Drop-in paths: /etc/systemd/system/systemd-nspawn@archlinux.service.d/90-lasper.conf",
            "Start command: /usr/bin/systemd-nspawn --boot",
        ] {
            assert!(rows.iter().any(|row| row == expected), "missing {expected}");
        }
        assert!(!rows.iter().any(|row| [
            "Load state:",
            "Active state:",
            "Substate:",
            "Active since:",
            "Result:",
            "Device allow:"
        ]
        .iter()
        .any(|label| row.starts_with(label))));
        properties.insert(GROUP_SYSTEMD_UNIT, "LoadState".into(), "bad-setting".into());
        properties.insert(GROUP_SYSTEMD_UNIT, "SubState".into(), "stop-sigterm".into());
        properties.insert(GROUP_SYSTEMD_SERVICE, "Result".into(), "exit-code".into());
        let rows = text(&machine_lines(&machine(), Ok(&properties), 120));
        assert!(rows.iter().any(|row| row == "Load state: bad-setting"));
        assert!(rows.iter().any(|row| row == "Substate: stop-sigterm"));
        assert!(rows.iter().any(|row| row == "Result: exit-code"));
        for width in [1, 8, 30] {
            assert!(machine_lines(&machine(), Ok(&properties), width)
                .iter()
                .all(|line| line.width() <= width));
        }
    }

    #[test]
    fn failed_inspection_keeps_identity_and_narrow_viewports_can_reach_the_error() {
        init_theme(Theme::dark());
        let machine = machine();
        let wide = text(&machine_lines(&machine, Err("permission denied"), 80));
        assert!(wide.iter().any(|line| line == "Name: archlinux"));
        assert!(wide.iter().any(|line| line == "Status: starting"));
        assert!(wide.last().unwrap().contains("permission denied"));
        assert!(!wide
            .iter()
            .any(|line| line == "Service" || line == "Resources"));

        for width in [1, 2, 8, 18, 80] {
            let lines = machine_lines(&machine, Err("permission denied"), width);
            assert!(lines.iter().all(|line| line.width() <= width));
            let area = Rect::new(0, 0, width as u16, 2);
            let offset = lines.len().saturating_sub(usize::from(area.height));
            let mut buffer = Buffer::empty(area);
            Paragraph::new(lines.clone())
                .scroll((offset as u16, 0))
                .render(area, &mut buffer);
            let actual: Vec<_> = buffer
                .content
                .chunks(width)
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            assert_eq!(actual, text(&lines[offset..]));
        }
        assert!(machine_lines(&machine, Err("permission denied"), 0).is_empty());
    }
}
