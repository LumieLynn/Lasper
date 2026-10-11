//! Compound property formats supported by systemctl's print_property.
//! Rules are selected by both property name and full D-Bus signature. New or
//! differently typed properties retain the generic representation.

use super::execution::signal_name;
use super::generic::{format_fields, format_string_array, format_yes_no, generic};
use super::scalar::{value_as_i32, value_as_u64};
use super::time::{format_duration_us, format_timestamp};
use crate::adapters::runtime::systemd_properties::format_ip_address;
use crate::domain::inspection::PropertyValue;

pub(super) fn format_allowlist(value: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = value else {
        return generic(value);
    };
    let fields = structure.as_slice();
    if fields.len() < 2 {
        return generic(value);
    }
    let allow = matches!(fields[0], PropertyValue::Bool(true));
    let values = format_string_array(&fields[1]);
    if values.is_empty() && allow {
        String::new()
    } else if allow {
        values
    } else if values.is_empty() {
        "~".to_string()
    } else {
        format!("~{values}")
    }
}

pub(super) fn format_optional_label(value: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = value else {
        return generic(value);
    };
    let fields = structure.as_slice();
    if fields.len() < 2 {
        return generic(value);
    }
    let ignored = matches!(fields[0], PropertyValue::Bool(true));
    let label = generic(&fields[1]);
    if label.is_empty() {
        String::new()
    } else if ignored {
        format!("-{label}")
    } else {
        label
    }
}

pub(super) fn format_exit_status_set(value: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = value else {
        return generic(value);
    };
    let fields = structure.as_slice();
    if fields.len() < 2 {
        return generic(value);
    }
    let mut values = format_integer_array(&fields[0]);
    let signals = if let PropertyValue::Array(array) = &fields[1] {
        array
            .iter()
            .filter_map(value_as_i32)
            .map(signal_name)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        generic(&fields[1])
    };
    if !signals.is_empty() {
        if !values.is_empty() {
            values.push(' ');
        }
        values.push_str(&signals);
    }
    if values.is_empty() {
        String::new()
    } else {
        values
    }
}

pub(super) fn format_directory_quota(value: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = value else {
        return generic(value);
    };
    let fields = structure.as_slice();
    if fields.len() < 3 {
        return generic(value);
    }
    let absolute = value_as_u64(&fields[0]).unwrap_or(u64::MAX);
    let scale = value_as_u64(&fields[1]).unwrap_or(0);
    let enforced = matches!(&fields[2], PropertyValue::String(s) if matches!(s.as_str(), "yes" | "true" | "on" | "1"));
    if !enforced {
        "[not set]".to_string()
    } else if absolute != u64::MAX {
        absolute.to_string()
    } else {
        let percent = (scale.saturating_mul(100) + u64::from(u32::MAX / 2)) / u64::from(u32::MAX);
        format!("{percent}%")
    }
}

pub(super) fn format_environment_files(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!(
            "{} (ignore_errors={})",
            generic(&fields[0]),
            format_yes_no(&fields[1])
        )
    })
}

pub(super) fn format_typed_path_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!("{} ({})", generic(&fields[1]), generic(&fields[0]))
    })
}

pub(super) fn format_monotonic_timer_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 3 {
            return format_fields(fields);
        }
        format!(
            "{{ {}={} ; next_elapse={} }}",
            generic(&fields[0]),
            format_duration_us(&fields[1]),
            format_duration_us(&fields[2])
        )
    })
}

pub(super) fn format_calendar_timer_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 3 {
            return format_fields(fields);
        }
        format!(
            "{{ {}={} ; next_elapse={} }}",
            generic(&fields[0]),
            generic(&fields[1]),
            format_timestamp(&fields[2])
        )
    })
}

pub(super) fn format_device_allow(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!("{} {}", generic(&fields[0]), generic(&fields[1]))
    })
}

pub(super) fn format_path_value_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!("{} {}", generic(&fields[0]), generic(&fields[1]))
    })
}

pub(super) fn format_path_duration_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!("{} {}", generic(&fields[0]), format_duration_us(&fields[1]))
    })
}

pub(super) fn format_ip_policy(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 3 {
            return format_fields(fields);
        }
        let family = value_as_i32(&fields[0]).unwrap_or_default();
        let address = match &fields[1] {
            PropertyValue::Array(array) => array
                .iter()
                .filter_map(|value| match value {
                    PropertyValue::U8(byte) => Some(*byte),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let prefix = value_as_u64(&fields[2]).unwrap_or_default();
        let address = format_ip_address(family, &address);
        if address.is_empty() {
            format_fields(fields)
        } else {
            format!("{address}/{prefix}")
        }
    })
}

pub(super) fn format_bind_path_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 4 {
            return format_fields(fields);
        }
        let source = generic(&fields[0]);
        if source.is_empty() {
            return String::new();
        }
        let destination = generic(&fields[1]);
        let ignore = matches!(fields[2], PropertyValue::Bool(true));
        let recursive = value_as_u64(&fields[3]) == Some(libc::MS_REC);
        format!(
            "{}{}{}{}",
            if ignore { "-" } else { "" },
            source,
            if destination.is_empty() {
                String::new()
            } else {
                format!(":{destination}")
            },
            if recursive { ":rbind" } else { "" }
        )
    })
}

pub(super) fn format_log_filter_patterns(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        let allow = matches!(fields[0], PropertyValue::Bool(true));
        let pattern = generic(&fields[1]);
        if allow {
            pattern
        } else {
            format!("~{pattern}")
        }
    })
}

pub(super) fn format_pair_list(value: &PropertyValue, separator: &str) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!(
            "{}{}{}",
            generic(&fields[0]),
            separator,
            generic(&fields[1])
        )
    })
}

pub(super) fn byte_array(value: &PropertyValue) -> Option<Vec<u8>> {
    let PropertyValue::Array(array) = value else {
        return None;
    };
    array
        .iter()
        .map(|value| match value {
            PropertyValue::U8(byte) => Some(*byte),
            _ => None,
        })
        .collect()
}

pub(super) fn format_base64_bytes(value: &PropertyValue) -> String {
    let Some(bytes) = byte_array(value) else {
        return generic(value);
    };
    if bytes.is_empty() {
        return String::new();
    }
    base64_encode(&bytes)
}

pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[((first & 0x03) << 4 | second >> 4) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((second & 0x0f) << 2 | third >> 6) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub(super) fn format_log_extra_fields(value: &PropertyValue) -> String {
    let PropertyValue::Array(fields) = value else {
        return generic(value);
    };
    let values = fields
        .iter()
        .filter_map(byte_array)
        .filter_map(|bytes| {
            let text = std::str::from_utf8(&bytes).ok()?;
            let (name, _) = text.split_once('=')?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return None;
            }
            Some(text.to_string())
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn nested_pair_options(value: &PropertyValue) -> Vec<String> {
    let PropertyValue::Array(options) = value else {
        return Vec::new();
    };
    options
        .iter()
        .filter_map(|option| {
            let PropertyValue::Structure(structure) = option else {
                return None;
            };
            let fields = structure.as_slice();
            (fields.len() >= 2).then(|| format!("{}:{}", generic(&fields[0]), generic(&fields[1])))
        })
        .collect()
}

pub(super) fn format_mount_images(value: &PropertyValue) -> String {
    let PropertyValue::Array(images) = value else {
        return generic(value);
    };
    let values = images
        .iter()
        .filter_map(|image| {
            let PropertyValue::Structure(structure) = image else {
                return None;
            };
            let fields = structure.as_slice();
            if fields.len() < 4 {
                return None;
            }
            let source = generic(&fields[0]);
            let destination = generic(&fields[1]);
            let prefix = matches!(fields[2], PropertyValue::Bool(true));
            let mut result = format!(
                "{}{}:{}",
                if prefix { "-" } else { "" },
                source,
                destination
            );
            for option in nested_pair_options(&fields[3]) {
                result.push(':');
                result.push_str(&option);
            }
            Some(result)
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn format_extension_images(value: &PropertyValue) -> String {
    let PropertyValue::Array(images) = value else {
        return generic(value);
    };
    let values = images
        .iter()
        .filter_map(|image| {
            let PropertyValue::Structure(structure) = image else {
                return None;
            };
            let fields = structure.as_slice();
            if fields.len() < 3 {
                return None;
            }
            let source = generic(&fields[0]);
            let prefix = matches!(fields[1], PropertyValue::Bool(true));
            let mut result = format!("{}{}", if prefix { "-" } else { "" }, source);
            for option in nested_pair_options(&fields[2]) {
                result.push(':');
                result.push_str(&option);
            }
            Some(result)
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn format_open_file_list(value: &PropertyValue) -> String {
    let PropertyValue::Array(files) = value else {
        return generic(value);
    };
    let values = files
        .iter()
        .filter_map(|file| {
            let PropertyValue::Structure(structure) = file else {
                return None;
            };
            let fields = structure.as_slice();
            if fields.len() < 3 {
                return None;
            }
            let path = generic(&fields[0]).replace(':', "\\x3a");
            let fd_name = generic(&fields[1]);
            let mut result = path.clone();
            if std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(fd_name.as_str())
            {
                result.push(':');
                result.push_str(&fd_name);
            }
            let flags = value_as_u64(&fields[2]).unwrap_or_default();
            let names = [
                (1u64, "read-only"),
                (2u64, "append"),
                (4u64, "truncate"),
                (8u64, "graceful"),
            ];
            let options = names
                .iter()
                .filter_map(|(bit, name)| (flags & bit != 0).then_some(*name))
                .collect::<Vec<_>>();
            if !options.is_empty() {
                if result == path {
                    result.push_str("::");
                } else {
                    result.push(':');
                }
                result.push_str(&options.join(","));
            }
            Some(result)
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn format_symlink_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 2 {
            return format_fields(fields);
        }
        format!("{}:{}", generic(&fields[0]), generic(&fields[1]))
    })
}

pub(super) fn format_socket_bind_list(value: &PropertyValue) -> String {
    format_struct_array(value, |fields| {
        if fields.len() < 4 {
            return format_fields(fields);
        }
        let family = value_as_i32(&fields[0]).unwrap_or_default();
        let protocol = value_as_i32(&fields[1]).unwrap_or_default();
        let count = value_as_u64(&fields[2]).unwrap_or_default();
        let first = value_as_u64(&fields[3]).unwrap_or_default();
        let family = match family {
            2 => "ipv4",
            10 => "ipv6",
            _ => "",
        };
        let protocol = match protocol {
            6 => "tcp",
            17 => "udp",
            _ => "",
        };
        let prefix = if family.is_empty() {
            String::new()
        } else {
            format!("{family}:")
        };
        let protocol = if protocol.is_empty() {
            String::new()
        } else {
            format!("{protocol}:")
        };
        let ports = match count {
            0 => "any".to_string(),
            1 => first.to_string(),
            _ => format!("{first}-{}", first.saturating_add(count - 1)),
        };
        format!("{prefix}{protocol}{ports}")
    })
}

pub(super) fn format_struct_array<F>(value: &PropertyValue, mut formatter: F) -> String
where
    F: FnMut(&[PropertyValue]) -> String,
{
    let PropertyValue::Array(array) = value else {
        return generic(value);
    };
    let values: Vec<String> = array
        .iter()
        .filter_map(|item| match item {
            PropertyValue::Structure(structure) => Some(formatter(structure.as_slice())),
            _ => None,
        })
        .filter(|item| !item.is_empty())
        .collect();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn format_integer_array(value: &PropertyValue) -> String {
    let PropertyValue::Array(array) = value else {
        return generic(value);
    };
    let values: Vec<String> = array
        .iter()
        .filter_map(|value| match value {
            PropertyValue::I32(value) => Some(value.to_string()),
            PropertyValue::U32(value) => Some(value.to_string()),
            PropertyValue::I64(value) => Some(value.to_string()),
            PropertyValue::U64(value) => Some(value.to_string()),
            _ => None,
        })
        .collect();
    if values.is_empty() {
        String::new()
    } else {
        values.join(" ")
    }
}

pub(super) fn format_job(v: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = v else {
        return generic(v);
    };
    let fields = structure.as_slice();
    let Some(job_id) = fields.first().and_then(value_as_u64) else {
        return generic(v);
    };
    if job_id == 0 {
        String::new()
    } else {
        job_id.to_string()
    }
}

pub(super) fn format_unit_reference(v: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = v else {
        return generic(v);
    };
    structure
        .as_slice()
        .first()
        .map(generic)
        .unwrap_or_else(|| generic(v))
}

pub(super) fn format_load_error(v: &PropertyValue) -> String {
    let PropertyValue::Structure(structure) = v else {
        return generic(v);
    };
    let fields = structure.as_slice();
    if fields.len() < 2 {
        return generic(v);
    }
    let name = generic(&fields[0]);
    let message = generic(&fields[1]);
    if name.is_empty() && message.is_empty() {
        String::new()
    } else {
        format!("{name} \"{message}\"")
    }
}

type Printer = fn(&PropertyValue) -> String;

const RULES: &[(&[&str], &str, Printer)] = &[
    (&["Job"], "(uo)", format_job),
    (&["Unit"], "(so)", format_unit_reference),
    (&["LoadError"], "(ss)", format_load_error),
    (
        &[
            "SystemCallFilter",
            "SystemCallLog",
            "RestrictAddressFamilies",
            "RestrictNetworkInterfaces",
            "RestrictFileSystems",
        ],
        "(bas)",
        format_allowlist,
    ),
    (
        &["SELinuxContext", "AppArmorProfile", "SmackProcessLabel"],
        "(bs)",
        format_optional_label,
    ),
    (
        &[
            "StateDirectoryQuota",
            "CacheDirectoryQuota",
            "LogsDirectoryQuota",
        ],
        "(tus)",
        format_directory_quota,
    ),
    (&["EnvironmentFiles"], "a(sb)", format_environment_files),
    (&["Paths", "Listen"], "a(ss)", format_typed_path_list),
    (&["TimersMonotonic"], "a(stt)", format_monotonic_timer_list),
    (&["TimersCalendar"], "a(sst)", format_calendar_timer_list),
    (&["DeviceAllow"], "a(ss)", format_device_allow),
    (
        &[
            "IODeviceWeight",
            "IOReadBandwidthMax",
            "IOWriteBandwidthMax",
            "IOReadIOPSMax",
            "IOWriteIOPSMax",
        ],
        "a(st)",
        format_path_value_list,
    ),
    (
        &["IODeviceLatencyTargetUSec"],
        "a(st)",
        format_path_duration_list,
    ),
    (
        &["StandardInputData", "RootHashSignature"],
        "ay",
        format_base64_bytes,
    ),
    (
        &["IPAddressAllow", "IPAddressDeny"],
        "a(iayu)",
        format_ip_policy,
    ),
    (
        &["BindPaths", "BindReadOnlyPaths"],
        "a(ssbt)",
        format_bind_path_list,
    ),
    (
        &["TemporaryFileSystem", "RootImageOptions", "BPFProgram"],
        "a(ss)",
        |v| format_pair_list(v, ":"),
    ),
    (&["LogExtraFields"], "aay", format_log_extra_fields),
    (&["MountImages"], "a(ssba(ss))", format_mount_images),
    (&["ExtensionImages"], "a(sba(ss))", format_extension_images),
    (&["LogFilterPatterns"], "a(bs)", format_log_filter_patterns),
    (
        &[
            "StateDirectorySymlink",
            "RuntimeDirectorySymlink",
            "CacheDirectorySymlink",
            "LogsDirectorySymlink",
        ],
        "a(sst)",
        format_symlink_list,
    ),
    (
        &[
            "StateDirectorySymlink",
            "RuntimeDirectorySymlink",
            "CacheDirectorySymlink",
            "LogsDirectorySymlink",
        ],
        "a(ss)",
        format_symlink_list,
    ),
    (
        &["SocketBindAllow", "SocketBindDeny"],
        "a(iiqq)",
        format_socket_bind_list,
    ),
    (&["OpenFile"], "a(sst)", format_open_file_list),
    (&["ExtraFileDescriptorNames"], "as", format_string_array),
    (
        &[
            "CPUAffinity",
            "NUMAMask",
            "AllowedCPUs",
            "AllowedMemoryNodes",
            "StartupAllowedCPUs",
            "StartupAllowedMemoryNodes",
            "EffectiveCPUs",
            "EffectiveMemoryNodes",
        ],
        "ay",
        super::bitmap::format_cpu_set,
    ),
];

pub(super) fn render(name: &str, value: &PropertyValue) -> Option<String> {
    if !matches!(value, PropertyValue::Structure(_) | PropertyValue::Array(_)) {
        return None;
    }
    let signature = value.signature();
    if name.starts_with("Exec")
        && matches!(signature.as_str(), "a(sasbttttuii)" | "a(sasasttttuii)")
    {
        return Some(super::execution::format_exec_command(value));
    }
    if name.ends_with("ExitStatus") && signature == "(aiai)" {
        return Some(format_exit_status_set(value));
    }
    RULES
        .iter()
        .find(|(names, expected, _)| names.contains(&name) && signature == *expected)
        .map(|(_, _, printer)| printer(value))
}
