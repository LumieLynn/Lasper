//! systemd timestamp and timespan presentation; never reinterprets text values.

use super::generic::generic;
use crate::domain::inspection::PropertyValue;
use std::ffi::CStr;

pub(super) fn format_timestamp(value: &PropertyValue) -> String {
    match value {
        PropertyValue::U64(value) => timestamp(*value, false).unwrap_or_else(|| "n/a".into()),
        _ => generic(value),
    }
}

pub(super) fn timestamp(microseconds: u64, utc: bool) -> Option<String> {
    if microseconds == 0 || microseconds == u64::MAX {
        return None;
    }
    let Ok(seconds) = (microseconds / 1_000_000).try_into() else {
        return None;
    };
    // libc supplies the host's local timezone, just as systemctl's default
    // timestamp style does. No time_t alias is needed on musl targets.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let result = if utc {
            libc::gmtime_r(&seconds, &mut tm)
        } else {
            libc::localtime_r(&seconds, &mut tm)
        };
        if result.is_null() {
            return None;
        }
        let mut buffer = [0u8; 128];
        let length = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%a %Y-%m-%d %H:%M:%S %Z".as_ptr(),
            &tm,
        );
        if length == 0 {
            None
        } else {
            Some(
                CStr::from_ptr(buffer.as_ptr().cast())
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
}

pub(super) fn format_duration_us(value: &PropertyValue) -> String {
    match value {
        PropertyValue::U64(value) => timespan(*value),
        _ => generic(value),
    }
}

pub(super) fn timespan(mut microseconds: u64) -> String {
    match microseconds {
        u64::MAX => return "infinity".into(),
        0 => return "0".into(),
        _ => {}
    }
    // Same descending units and dot-notation rule as format_timespan(t, 0).
    const UNITS: &[(&str, u64)] = &[
        ("y", 31_557_600_000_000),
        ("month", 2_629_800_000_000),
        ("w", 604_800_000_000),
        ("d", 86_400_000_000),
        ("h", 3_600_000_000),
        ("min", 60_000_000),
        ("s", 1_000_000),
        ("ms", 1_000),
        ("us", 1),
    ];
    let mut parts = Vec::new();
    for (suffix, unit) in UNITS {
        if microseconds < *unit {
            continue;
        }
        let whole = microseconds / unit;
        let remainder = microseconds % unit;
        if microseconds < 60_000_000 && remainder > 0 {
            let precision = unit.ilog10() as usize;
            parts.push(format!("{whole}.{remainder:0precision$}{suffix}"));
            break;
        }
        parts.push(format!("{whole}{suffix}"));
        microseconds = remainder;
        if microseconds == 0 {
            break;
        }
    }
    parts.join(" ")
}
