use crate::nspawn::models::MachineProperties;
use std::ffi::CStr;
use zbus::zvariant::Value;

/// Keys that should be routed to the Dependencies group instead of Systemd.
const DEPENDENCY_KEYS: &[&str] = &[
    "After",
    "Before",
    "Wants",
    "WantedBy",
    "Requires",
    "RequiredBy",
    "Conflicts",
    "ConflictedBy",
];

pub fn is_dependency_key(key: &str) -> bool {
    DEPENDENCY_KEYS.contains(&key)
}

/// Insert a formatted systemd unit property into the correct group.
pub fn insert_systemd_property(props: &mut MachineProperties, key: String, value: String) {
    if is_dependency_key(&key) {
        if !value.is_empty() && value != "[]" {
            props.insert(crate::nspawn::models::GROUP_DEPENDENCIES, key, value);
        }
    } else {
        props.insert(crate::nspawn::models::GROUP_SYSTEMD_UNIT, key, value);
    }
}

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

        // UUID-like byte arrays -> hex string
        "Id" | "InvocationID" | "MachineID" => format_id(value),

        // Timestamps (Type 't' - microseconds since epoch)
        // Exclude monotonic timestamps (microseconds since boot) — systemd shows those raw.
        k if (k.contains("Timestamp") && !k.contains("Monotonic"))
            || (k.ends_with("Time") && !k.contains("Monotonic")) =>
        {
            format_timestamp(value)
        }

        // Sizes (Bytes)
        "MemoryCurrent" | "MemoryMax" | "MemoryLimit" | "MemoryAvailable" | "MemoryHigh"
        | "MemoryLow" | "IOWriteBytes" | "IOReadBytes" | "Usage" => format_size_value(value),

        // Microsecond durations (systemd *USec properties)
        k if k.ends_with("USec") => format_duration_us(value),

        // Durations (Nanoseconds)
        "CPUUsageNS" => format_duration_ns(value),

        // Dependency Filtration
        "After" | "Before" | "Wants" | "WantedBy" | "Requires" | "RequiredBy" | "Conflicts"
        | "ConflictedBy" => format_dependencies(value),

        // ExecCommand structures (systemd's custom serialization).
        // ExecMain* keys are metadata (timestamps, PID, code) — not commands.
        k if k.starts_with("Exec") && !k.starts_with("ExecMain") => format_exec_command(value),

        // Fallback to type-based formatting
        _ => format_dbus_value(value),
    }
}

/// Recursively formats a D-Bus Value into a human-readable, systemd-style string.
pub fn format_dbus_value(v: &Value<'_>) -> String {
    match v {
        Value::Str(s) => {
            if s.is_empty() {
                "[not set]".to_string()
            } else {
                s.as_str().to_string()
            }
        }
        Value::U8(n) => n.to_string(),
        Value::I16(n) => n.to_string(),
        Value::U16(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::U32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => {
            if *n == u64::MAX {
                "infinity".to_string()
            } else {
                n.to_string()
            }
        }
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
            if arr.is_empty() {
                return "[not set]".to_string();
            }

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

            let formatted: Vec<String> = arr.iter().map(|v| format_dbus_value(v)).collect();
            formatted.join(" ")
        }

        Value::Dict(d) => d
            .iter()
            .map(|(k, v)| format!("{}={}", format_dbus_value(k), format_dbus_value(v)))
            .collect::<Vec<String>>()
            .join(", "),

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
    if raw.is_empty() || raw == "[]" || raw == "[not set]" {
        return String::new();
    }

    let units: Vec<&str> = raw.split_whitespace().collect();
    let original_count = units.len();

    let mut filtered: Vec<&str> = units
        .into_iter()
        .filter(|u| !DEPENDENCY_BLOCKLIST.contains(u))
        .collect();

    let hidden_count = original_count - filtered.len();

    if filtered.is_empty() && hidden_count > 0 {
        return "(system default)".to_string();
    }

    filtered.sort_unstable();

    let mut result = filtered.join(" ");
    if hidden_count > 0 {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(&format!("(+ {} system units)", hidden_count));
    }

    result
}

/// Format an ExecCommand struct (from CLI serialization or D-Bus typed data).
/// Produces just the command line; metadata (pid, exit code, timestamps) live in
/// separate ExecMain* properties.
fn format_exec_command(v: &Value<'_>) -> String {
    match v {
        Value::Str(raw) => {
            if raw.is_empty() {
                return "[not set]".to_string();
            }
            parse_exec_commands(raw)
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[not set]".to_string();
            }
            let commands: Vec<String> = arr
                .iter()
                .map(|elem| {
                    if let Value::Structure(s) = elem {
                        let fields = s.fields();
                        if fields.len() >= 2 {
                            if let Value::Array(ref argv_arr) = fields[1] {
                                let args: Vec<String> =
                                    argv_arr.iter().map(|a| format_dbus_value(a)).collect();
                                return args.join(" ");
                            }
                        }
                        if !fields.is_empty() {
                            return format_dbus_value(&fields[0]);
                        }
                    }
                    format_dbus_value(elem)
                })
                .filter(|s| !s.is_empty())
                .collect();
            if commands.is_empty() {
                "[not set]".to_string()
            } else {
                commands.join("\n")
            }
        }
        _ => format_dbus_value(v),
    }
}

/// Parse systemd's custom exec-command serialization.
/// Format: `{ path=... ; argv[]=command arg1 arg2 ; ... }`
/// Multiple commands are simply concatenated: `{...} {...}`
fn parse_exec_commands(raw: &str) -> String {
    let mut commands: Vec<String> = Vec::new();
    let mut remaining = raw.trim();

    while let Some(start) = remaining.find('{') {
        let block_start = start + 1;
        let mut depth = 1u32;
        let mut block_end = None;

        for (i, ch) in remaining[block_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = Some(block_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }

        match block_end {
            Some(end) => {
                commands.push(extract_command_line(&remaining[block_start..end]));
                remaining = &remaining[end + 1..];
            }
            None => break,
        }
    }

    if commands.is_empty() {
        raw.to_string()
    } else {
        commands.join("\n")
    }
}

/// From a single exec-command block body (inside the `{ }`), extract the argv line.
fn extract_command_line(block: &str) -> String {
    for part in block.split(" ; ") {
        if let Some(val) = part.trim().strip_prefix("argv[]=") {
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }

    // Fallback: use path= if no argv[]
    for part in block.split(" ; ") {
        if let Some(val) = part.trim().strip_prefix("path=") {
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }

    block.to_string()
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
                format_dbus_value(item)
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

    if ns == u64::MAX {
        return "infinity".to_string();
    }

    let total_secs = ns / 1_000_000_000;
    if total_secs == 0 {
        return format!("{}ms", ns / 1_000_000);
    }

    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;

    if h > 0 {
        format!("{}h {}min {}s", h, m, s)
    } else if m > 0 {
        format!("{}min {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Formats a microsecond duration in systemd's `format_timespan` style.
fn format_duration_us(v: &Value<'_>) -> String {
    let us = match v {
        Value::U64(n) => *n,
        Value::U32(n) => *n as u64,
        _ => return format_dbus_value(v),
    };

    if us == u64::MAX {
        return "infinity".to_string();
    }

    if us == 0 {
        return "0".to_string();
    }

    if us < 1000 {
        return format!("{}us", us);
    }

    if us < 1_000_000 {
        let ms = us / 1000;
        let rem = us % 1000;
        if rem == 0 {
            return format!("{}ms", ms);
        }
        // Strip trailing zeros from the fractional part
        let mut frac = format!("{:03}", rem);
        while frac.ends_with('0') {
            frac.pop();
        }
        return format!("{}.{}ms", ms, frac);
    }

    let total_secs = us / 1_000_000;
    let rem_us = us % 1_000_000;

    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;

    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(format!("{}h", h));
    }
    if m > 0 {
        parts.push(format!("{}min", m));
    }
    if s > 0 || parts.is_empty() {
        if rem_us > 0 && h == 0 && m == 0 {
            let mut frac = format!("{:06}", rem_us);
            while frac.ends_with('0') {
                frac.pop();
            }
            parts.push(format!("{}.{}s", s, frac));
        } else {
            parts.push(format!("{}s", s));
        }
    }
    parts.join(" ")
}

/// Formats a 128-bit machine Id from its raw bytes into a lowercase hex string.
/// Falls back to `format_dbus_value` for non-byte-array values (e.g. unit Id strings).
fn format_id(v: &Value<'_>) -> String {
    if let Value::Array(arr) = v {
        if arr.element_signature() == "y" {
            return arr
                .iter()
                .filter_map(|b| {
                    if let Value::U8(b) = b {
                        Some(format!("{:02x}", b))
                    } else {
                        None
                    }
                })
                .collect();
        }
    }
    format_dbus_value(v)
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

    let secs = (us / 1_000_000) as libc::time_t;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);

        let mut buf: [u8; 128] = [0; 128];
        let len = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            c"%a %Y-%m-%d %H:%M:%S %Z".as_ptr(),
            &tm,
        );

        if len > 0 {
            CStr::from_ptr(buf.as_ptr() as *const libc::c_char)
                .to_string_lossy()
                .into_owned()
        } else {
            format!("{}s (unix epoch)", secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Size formatting

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(1024 * 1024), "1.0M");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024 * 1024), "2.0T");
        assert_eq!(format_size(u64::MAX), "infinity");
    }

    #[test]
    fn test_format_size_value_u32_fallback() {
        let val = Value::U32(2048);
        assert_eq!(format_size_value(&val), "2.0K");
    }

    #[test]
    fn test_format_size_value_non_numeric_fallback() {
        let val = Value::Str("not a number".into());
        assert_eq!(format_size_value(&val), "not a number");
    }

    // IP formatting

    #[test]
    fn test_format_ip_v4() {
        assert_eq!(format_ip_address(2, &[192, 168, 1, 1]), "192.168.1.1");
        assert_eq!(format_ip_address(2, &[0, 0, 0, 0]), "0.0.0.0");
        assert_eq!(
            format_ip_address(2, &[255, 255, 255, 255]),
            "255.255.255.255"
        );
    }

    #[test]
    fn test_format_ip_v4_wrong_length() {
        assert_eq!(format_ip_address(2, &[192, 168, 1]), "");
        assert_eq!(format_ip_address(2, &[192, 168, 1, 1, 5]), "");
        assert_eq!(format_ip_address(2, &[]), "");
    }

    #[test]
    fn test_format_ip_v6() {
        let data = vec![
            0x20, 0x01, 0x0d, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ];
        assert_eq!(format_ip_address(10, &data), "2001:db8:0:0:0:0:0:1");
    }

    #[test]
    fn test_format_ip_v6_wrong_length() {
        assert_eq!(format_ip_address(10, &[0x20, 0x01]), "");
        assert_eq!(format_ip_address(10, &[]), "");
    }

    #[test]
    fn test_format_ip_unknown_family() {
        assert_eq!(format_ip_address(99, &[1, 2, 3]), "[3 bytes]");
        assert_eq!(format_ip_address(0, &[]), "[0 bytes]");
    }

    // Duration formatting (nanoseconds)

    #[test]
    fn test_format_duration_ns() {
        assert_eq!(format_duration_ns(&Value::U64(1_500_000_000)), "1s");
        assert_eq!(
            format_duration_ns(&Value::U64(3_661_000_000_000)),
            "1h 1min 1s"
        );
        assert_eq!(format_duration_ns(&Value::U64(500_000_000)), "500ms");
    }

    #[test]
    fn test_format_duration_ns_zero() {
        assert_eq!(format_duration_ns(&Value::U64(0)), "0ms");
    }

    #[test]
    fn test_format_duration_ns_infinity() {
        assert_eq!(format_duration_ns(&Value::U64(u64::MAX)), "infinity");
    }

    #[test]
    fn test_format_duration_ns_exact_minutes() {
        assert_eq!(format_duration_ns(&Value::U64(120_000_000_000)), "2min 0s");
    }

    #[test]
    fn test_format_duration_ns_u32_fallback() {
        assert_eq!(format_duration_ns(&Value::U32(2_000_000_000)), "2s");
    }

    #[test]
    fn test_format_duration_ns_non_numeric() {
        assert_eq!(format_duration_ns(&Value::Str("garbage".into())), "garbage");
    }

    // Duration formatting (microseconds, *USec properties)

    #[test]
    fn test_format_duration_us_infinity() {
        assert_eq!(format_duration_us(&Value::U64(u64::MAX)), "infinity");
    }

    #[test]
    fn test_format_duration_us_zero() {
        assert_eq!(format_duration_us(&Value::U64(0)), "0");
    }

    #[test]
    fn test_format_duration_us_microseconds() {
        assert_eq!(format_duration_us(&Value::U64(500)), "500us");
    }

    #[test]
    fn test_format_duration_us_milliseconds() {
        assert_eq!(format_duration_us(&Value::U64(5_000)), "5ms");
        assert_eq!(format_duration_us(&Value::U64(5_500)), "5.5ms");
    }

    #[test]
    fn test_format_duration_us_seconds() {
        // StartLimitIntervalUSec from real systemd output
        assert_eq!(format_duration_us(&Value::U64(10_000_000)), "10s");
    }

    #[test]
    fn test_format_duration_us_minutes_seconds() {
        assert_eq!(format_duration_us(&Value::U64(90_000_000)), "1min 30s");
    }

    #[test]
    fn test_format_duration_us_hours() {
        // Exactly 1 hour — no 0s suffix (matching systemd style)
        assert_eq!(format_duration_us(&Value::U64(3_600_000_000)), "1h");
    }

    #[test]
    fn test_format_duration_us_fractional_seconds() {
        // 1.5s = 1_500_000 us
        assert_eq!(format_duration_us(&Value::U64(1_500_000)), "1.5s");
    }

    #[test]
    fn test_format_duration_us_u32_fallback() {
        assert_eq!(format_duration_us(&Value::U32(10_000_000)), "10s");
    }

    #[test]
    fn test_format_duration_us_non_numeric() {
        assert_eq!(format_duration_us(&Value::Str("garbage".into())), "garbage");
    }

    #[test]
    fn test_format_duration_us_via_format_property() {
        // Verify that *USec keys route through format_duration_us
        let val = Value::U64(10_000_000);
        assert_eq!(format_property("StartLimitIntervalUSec", &val), "10s");
        assert_eq!(
            format_property("JobRunningTimeoutUSec", &Value::U64(u64::MAX)),
            "infinity"
        );
    }

    // Id formatting

    #[test]
    fn test_format_id_byte_array() {
        let bytes: Vec<u8> = vec![0x45, 0x0e, 0x2e, 0x15];
        let val = Value::Array(zbus::zvariant::Array::from(bytes));
        assert_eq!(format_id(&val), "450e2e15");
    }

    #[test]
    fn test_format_id_string_passthrough() {
        let val = Value::Str("systemd-nspawn@ubuntu-LTS.service".into());
        assert_eq!(format_id(&val), "systemd-nspawn@ubuntu-LTS.service");
    }

    // Timestamp formatting

    #[test]
    fn test_format_timestamp_zero() {
        assert_eq!(format_timestamp(&Value::U64(0)), "n/a");
    }

    #[test]
    fn test_format_timestamp_nonzero_is_human_readable() {
        // 2024-04-18 06:12:55 UTC = 1713415975 seconds since epoch
        let result = format_timestamp(&Value::U64(1713415975000000));
        // The exact output depends on local timezone, but should contain
        // the date components and NOT look like raw epoch seconds.
        assert!(
            result.contains("2024"),
            "expected year in result: {}",
            result
        );
        assert!(
            result.contains("04"),
            "expected month in result: {}",
            result
        );
        assert!(
            !result.contains("unix epoch"),
            "should not be raw epoch: {}",
            result
        );
        assert!(
            !result.ends_with('s'),
            "should not end with 's': {}",
            result
        );
    }

    #[test]
    fn test_format_timestamp_u32_fallback() {
        assert_eq!(format_timestamp(&Value::U32(0)), "n/a");
        let result = format_timestamp(&Value::U32(5_000_000));
        // 5 seconds after epoch = some date in 1970, but should be human-readable
        assert!(!result.contains("unix epoch"));
    }

    #[test]
    fn test_format_timestamp_non_numeric() {
        assert_eq!(format_timestamp(&Value::Str("garbage".into())), "garbage");
    }

    #[test]
    fn test_monotonic_timestamps_not_formatted() {
        // TimestampMonotonic should NOT be caught by the timestamp matcher
        use super::format_property;
        let val = Value::U64(90847270);
        let result = format_property("TimestampMonotonic", &val);
        assert_eq!(result, "90847270");
    }

    #[test]
    fn test_format_dbus_value_empty_string_is_not_set() {
        assert_eq!(format_dbus_value(&Value::Str("".into())), "[not set]");
    }

    #[test]
    fn test_format_dbus_value_empty_array_is_not_set() {
        let arr = zbus::zvariant::Array::from(Vec::<String>::new());
        assert_eq!(format_dbus_value(&Value::Array(arr)), "[not set]");
    }

    #[test]
    fn test_format_dbus_value_nonempty_string_passthrough() {
        assert_eq!(format_dbus_value(&Value::Str("hello".into())), "hello");
    }

    #[test]
    fn test_format_dbus_value_u64_max_is_infinity() {
        assert_eq!(format_dbus_value(&Value::U64(u64::MAX)), "infinity");
    }

    #[test]
    fn test_format_dbus_value_u64_normal() {
        assert_eq!(format_dbus_value(&Value::U64(42)), "42");
    }

    // Dependency filtering

    #[test]
    fn test_format_dependencies_filtration() {
        let val = Value::Str("basic.target my-app.service systemd-journald.socket".into());
        let result = format_dependencies(&val);
        assert!(result.contains("my-app.service"));
        assert!(result.contains("(+ 2 system units)"));
    }

    #[test]
    fn test_format_dependencies_all_blocklisted() {
        let val = Value::Str("basic.target sysinit.target".into());
        let result = format_dependencies(&val);
        assert_eq!(result, "(system default)");
    }

    #[test]
    fn test_format_dependencies_empty() {
        let val = Value::Str("".into());
        let result = format_dependencies(&val);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_dependencies_no_blocklisted() {
        let val = Value::Str("my-app.service my-db.service".into());
        let result = format_dependencies(&val);
        assert_eq!(result, "my-app.service my-db.service");
    }

    // Exec command formatting

    #[test]
    fn test_format_exec_command_single() {
        let raw = "{ path=systemd-nspawn ; argv[]=systemd-nspawn --quiet --boot --machine=ubuntu-LTS ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        let result = format_exec_command(&Value::Str(raw.into()));
        assert_eq!(result, "systemd-nspawn --quiet --boot --machine=ubuntu-LTS");
    }

    #[test]
    fn test_format_exec_command_extended() {
        // Ex variants have an extra `flags=` field
        let raw = "{ path=systemd-nspawn ; argv[]=systemd-nspawn --cleanup --machine=ubuntu-LTS ; flags= ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        let result = format_exec_command(&Value::Str(raw.into()));
        assert_eq!(result, "systemd-nspawn --cleanup --machine=ubuntu-LTS");
    }

    #[test]
    fn test_format_exec_command_falls_back_to_path() {
        let raw = "{ path=/usr/bin/foo ; ignore_errors=yes ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        let result = format_exec_command(&Value::Str(raw.into()));
        assert_eq!(result, "/usr/bin/foo");
    }

    #[test]
    fn test_format_exec_command_multiple() {
        let raw =
            "{ path=/bin/foo ; argv[]=/bin/foo arg1 } { path=/bin/bar ; argv[]=/bin/bar arg2 }";
        let result = format_exec_command(&Value::Str(raw.into()));
        assert_eq!(result, "/bin/foo arg1\n/bin/bar arg2");
    }

    #[test]
    fn test_format_exec_command_empty() {
        let result = format_exec_command(&Value::Str("".into()));
        assert_eq!(result, "[not set]");
    }

    #[test]
    fn test_format_exec_command_empty_dbus_array() {
        // Empty D-Bus array (no exec commands configured) → [not set]
        let arr = zbus::zvariant::Array::from(Vec::<String>::new());
        let result = format_exec_command(&Value::Array(arr));
        assert_eq!(result, "[not set]");
    }

    #[test]
    fn test_execmain_not_routed_to_exec_command() {
        // ExecMain* keys should NOT be parsed as exec commands
        let val = Value::Str("2463".into());
        let result = format_property("ExecMainPID", &val);
        assert_eq!(result, "2463");
    }

    #[test]
    fn test_format_exec_command_via_format_property() {
        let raw = "{ path=systemd-nspawn ; argv[]=systemd-nspawn --machine=test ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        let result = format_property("ExecStart", &Value::Str(raw.into()));
        assert_eq!(result, "systemd-nspawn --machine=test");
    }
}
