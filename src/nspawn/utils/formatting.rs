use zbus::zvariant::Value;

const DEPENDENCY_BLOCKLIST: &[&str] = &[
    "basic.target",
    "sysinit.target",
    "shutdown.target",
    "paths.target",
    "slices.target",
    "sockets.target",
    "timers.target",
    "cryptsetup.target",
    "remote-fs.target",
    "local-fs.target",
    "machines.target",
    "system.slice",
    "machine.slice",
    "-.slice",
    "-.mount",
    "systemd-journald.socket",
    "systemd-journald-dev-log.socket",
    "systemd-journald-audit.socket",
    "systemd-tmpfiles-setup.service",
    "systemd-modules-load.service",
    "modprobe@tun.service",
    "modprobe@loop.service",
    "modprobe@dm_mod.service",
];

/// Smart formatter that understands systemd property semantics.
pub fn format_property(key: &str, value: &Value<'_>) -> String {
    match key {
        "IPAddresses" => format_ip_addresses(value),

        // Timestamps (Type 't' - microseconds since epoch)
        k if k.contains("Timestamp") || k.ends_with("Time") => format_timestamp(value),

        // Sizes (Bytes)
        "MemoryCurrent"
        | "MemoryMax"
        | "MemoryLimit"
        | "MemoryAvailable"
        | "MemoryHigh"
        | "MemoryLow"
        | "IOWriteBytes"
        | "IOReadBytes"
        | "Usage" => format_size_value(value),

        // Durations (Nanoseconds)
        "CPUUsageNS" => format_duration_ns(value),

        // Dependency Filtration
        "After" | "Before" | "Wants" | "WantedBy" | "Requires" | "RequiredBy" | "Conflicts"
        | "ConflictedBy" => format_dependencies(value),

        // Fallback to type-based formatting
        _ => format_dbus_value(value),
    }
}

/// Recursively formats a D-Bus Value into a human-readable, systemd-style string.
pub fn format_dbus_value(v: &Value<'_>) -> String {
    match v {
        Value::Str(s) => s.as_str().to_string(),
        Value::U8(n) => n.to_string(),
        Value::I16(n) => n.to_string(),
        Value::U16(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::U32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "yes".to_string()
            } else {
                "no".to_string()
            }
        }
        Value::ObjectPath(p) => p.as_str().to_string(),
        Value::Signature(s) => s.as_str().to_string(),

        Value::Array(arr) => {
            // Special Case: Byte Arrays (Signature "y")
            if arr.element_signature() == "y" {
                let bytes: Vec<String> = arr
                    .iter()
                    .map(|v| match v {
                        Value::U8(b) => b.to_string(),
                        _ => String::new(),
                    })
                    .collect();
                return format!("[{}]", bytes.join(" "));
            }

            arr.iter()
                .map(|v| format_dbus_value(&v))
                .collect::<Vec<String>>()
                .join(" ")
        }

        Value::Dict(d) => {
            d.iter()
                .map(|(k, v)| format!("{}={}", format_dbus_value(&k), format_dbus_value(&v)))
                .collect::<Vec<String>>()
                .join(", ")
        }

        Value::Structure(s) => {
            let fields = s.fields();
            let mut formatted = Vec::new();
            for f in fields {
                formatted.push(format_dbus_value(f));
            }
            format!("({})", formatted.join(", "))
        }

        Value::Value(v) => format_dbus_value(v),
        Value::Fd(fd) => format!("<fd {:?}>", fd),
    }
}

// --- Standalone Helpers ---

/// Formats a raw byte count into a human-readable string with units (K, M, G, T).
pub fn format_size(bytes: u64) -> String {
    if bytes == u64::MAX {
        return "infinity".to_string();
    }

    const KI_B: u64 = 1024;
    const MI_B: u64 = KI_B * 1024;
    const GI_B: u64 = MI_B * 1024;
    const TI_B: u64 = GI_B * 1024;

    if bytes >= TI_B {
        format!("{:.1}T", bytes as f64 / TI_B as f64)
    } else if bytes >= GI_B {
        format!("{:.1}G", bytes as f64 / GI_B as f64)
    } else if bytes >= MI_B {
        format!("{:.1}M", bytes as f64 / MI_B as f64)
    } else if bytes >= KI_B {
        format!("{:.1}K", bytes as f64 / KI_B as f64)
    } else {
        format!("{}B", bytes)
    }
}

/// Formats raw IP family and bytes into a string.
pub fn format_ip_address(family: i32, data: &[u8]) -> String {
    match family {
        2 => {
            // AF_INET
            if data.len() == 4 {
                format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3])
            } else {
                String::new()
            }
        }
        10 => {
            // AF_INET6
            if data.len() == 16 {
                let mut s = String::new();
                for i in 0..8 {
                    if i > 0 {
                        s.push(':');
                    }
                    s.push_str(&format!(
                        "{:x}",
                        u16::from_be_bytes([data[i * 2], data[i * 2 + 1]])
                    ));
                }
                s
            } else {
                String::new()
            }
        }
        _ => format!("[{} bytes]", data.len()),
    }
}

// --- Specialized Handlers ---

fn format_dependencies(v: &Value<'_>) -> String {
    let raw = format_dbus_value(v);
    if raw.is_empty() || raw == "[]" {
        return String::new();
    }

    let units: Vec<&str> = raw.split_whitespace().collect();
    let original_count = units.len();

    let filtered: Vec<&str> = units
        .into_iter()
        .filter(|u| !DEPENDENCY_BLOCKLIST.contains(u))
        .collect();

    let hidden_count = original_count - filtered.len();

    if filtered.is_empty() && hidden_count > 0 {
        return "(system default)".to_string();
    }

    let mut result = filtered.join(" ");
    if hidden_count > 0 {
        if !result.is_empty() {
            result.push_str(" ");
        }
        result.push_str(&format!("(+ {} system units)", hidden_count));
    }

    result
}

fn format_ip_addresses(v: &Value<'_>) -> String {
    if let Value::Array(arr) = v {
        arr.iter()
            .map(|item| {
                if let Value::Structure(s) = item {
                    let fields = s.fields();
                    if fields.len() >= 2 {
                        let family = match fields[0] {
                            Value::I32(f) => f,
                            _ => 0,
                        };
                        if let Value::Array(ref addr_arr) = fields[1] {
                            let bytes: Vec<u8> = addr_arr
                                .iter()
                                .filter_map(|b| if let Value::U8(x) = b { Some(*x) } else { None })
                                .collect();
                            return format_ip_address(family, &bytes);
                        }
                    }
                }
                format_dbus_value(&item)
            })
            .collect::<Vec<String>>()
            .join(" ")
    } else {
        format_dbus_value(v)
    }
}

fn format_size_value(v: &Value<'_>) -> String {
    let bytes = match v {
        Value::U64(n) => *n,
        Value::U32(n) => *n as u64,
        _ => return format_dbus_value(v),
    };

    format_size(bytes)
}

fn format_duration_ns(v: &Value<'_>) -> String {
    let ns = match v {
        Value::U64(n) => *n,
        Value::U32(n) => *n as u64,
        _ => return format_dbus_value(v),
    };

    let total_secs = ns / 1_000_000_000;
    if total_secs == 0 {
        return format!("{}ms", ns / 1_000_000);
    }

    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;

    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

fn format_timestamp(v: &Value<'_>) -> String {
    let us = match v {
        Value::U64(n) => *n,
        Value::U32(n) => *n as u64,
        _ => return format_dbus_value(v),
    };

    if us == 0 {
        return "n/a".to_string();
    }

    format!("{}s (unix epoch)", us / 1_000_000)
}
