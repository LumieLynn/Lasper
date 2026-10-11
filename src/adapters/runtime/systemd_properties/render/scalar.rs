//! Scalar semantics from systemd's bus-print-properties.c and systemctl-show.c.
//! A name rule is considered only after its wire type has been established.

use super::bitmap::{format_capability_set, format_mount_flags, format_namespace_restriction};
use super::time::{timespan, timestamp};
use crate::domain::inspection::PropertyValue;

pub(super) fn render(name: &str, value: &PropertyValue) -> Option<String> {
    match value {
        PropertyValue::U64(number) => Some(unsigned64(name, *number, value)),
        PropertyValue::U32(number) => Some(if name.contains("UMask") || name.contains("Mode") {
            format!("{number:04o}")
        } else if matches!(name, "UID" | "GID") && *number == u32::MAX {
            "[not set]".into()
        } else {
            number.to_string()
        }),
        PropertyValue::I32(number) if name.ends_with("ActionExitStatus") => {
            Some(if (0..=255).contains(number) {
                number.to_string()
            } else {
                "[not set]".into()
            })
        }
        PropertyValue::I32(number) if name == "NUMAPolicy" => Some(match number {
            0 => "default".into(),
            1 => "preferred".into(),
            2 => "bind".into(),
            3 => "interleave".into(),
            4 => "local".into(),
            5 => "preferred-many".into(),
            6 => "weighted-interleave".into(),
            _ => "n/a".into(),
        }),
        _ => None,
    }
}

fn unsigned64(name: &str, number: u64, value: &PropertyValue) -> String {
    if name.ends_with("Timestamp")
        || matches!(
            name,
            "NextElapseUSecRealtime" | "LastTriggerUSec" | "TimeUSec" | "RTCTimeUSec"
        )
    {
        return timestamp(number, name == "RTCTimeUSec").unwrap_or_default();
    }
    if name == "ManagedOOMMemoryPressureDurationUSec" && number == u64::MAX {
        return "[not set]".into();
    }
    if name.contains("USec") {
        return timespan(number);
    }
    match name {
        "CoredumpFilter" => return format!("0x{number:x}"),
        "RestrictNamespaces" => return format_namespace_restriction(value),
        "MountFlags" => return format_mount_flags(value),
        "CapabilityBoundingSet" | "AmbientCapabilities" => return format_capability_set(value),
        "CPUWeight" | "StartupCPUWeight" if number == 0 => return "idle".into(),
        _ => {}
    }
    if number == u64::MAX {
        if matches!(
            name,
            "CPUWeight"
                | "StartupCPUWeight"
                | "IOWeight"
                | "StartupIOWeight"
                | "MemoryCurrent"
                | "MemoryAvailable"
                | "TasksCurrent"
        ) || (name.starts_with("Memory") && ends_with_any(name, &["Current", "Peak"]))
            || (name.starts_with("IO") && ends_with_any(name, &["Bytes", "Operations"]))
            || name.ends_with("NSec")
        {
            return "[not set]".into();
        }
        if ends_with_any(
            name,
            &[
                "MemoryLow",
                "MemoryMin",
                "MemoryHigh",
                "MemoryMax",
                "MemorySwapMax",
                "MemoryZSwapMax",
                "MemoryLimit",
                "TasksMax",
            ],
        ) || name.starts_with("Limit")
            || name.starts_with("DefaultLimit")
        {
            return "infinity".into();
        }
        if matches!(
            name,
            "IPIngressBytes" | "IPIngressPackets" | "IPEgressBytes" | "IPEgressPackets"
        ) {
            return "[no data]".into();
        }
    }
    number.to_string()
}

fn ends_with_any(name: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| name.ends_with(suffix))
}

pub(super) fn value_as_u64(value: &PropertyValue) -> Option<u64> {
    match value {
        PropertyValue::U8(number) => Some(u64::from(*number)),
        PropertyValue::U16(number) => Some(u64::from(*number)),
        PropertyValue::U32(number) => Some(u64::from(*number)),
        PropertyValue::U64(number) => Some(*number),
        _ => None,
    }
}

pub(super) fn value_as_i32(value: &PropertyValue) -> Option<i32> {
    match value {
        PropertyValue::I32(number) => Some(*number),
        PropertyValue::I16(number) => Some(i32::from(*number)),
        PropertyValue::U8(number) => Some(i32::from(*number)),
        PropertyValue::U16(number) => Some(i32::from(*number)),
        PropertyValue::U32(number) => i32::try_from(*number).ok(),
        _ => None,
    }
}
