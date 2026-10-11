//! Exec command/status values; scalar ExecMain properties use the generic printer.

use super::generic::{format_fields, format_string_array, format_yes_no, generic};
use super::scalar::value_as_i32;
use super::time::format_timestamp;
use crate::domain::inspection::PropertyValue;

pub(super) fn format_exec_command(v: &PropertyValue) -> String {
    match v {
        PropertyValue::String(raw) => {
            if raw.is_empty() {
                return String::new();
            }
            raw.as_str().to_string()
        }
        PropertyValue::Array(arr) => {
            if arr.is_empty() {
                return String::new();
            }
            let commands: Vec<String> = arr
                .iter()
                .filter_map(|elem| {
                    if let PropertyValue::Structure(s) = elem {
                        Some(format_exec_status(s.as_slice()))
                    } else {
                        None
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            if commands.is_empty() {
                String::new()
            } else {
                commands.join(" ")
            }
        }
        _ => generic(v),
    }
}

pub(super) fn format_exec_status(fields: &[PropertyValue]) -> String {
    if fields.len() < 10 {
        return format_fields(fields);
    }

    let path = generic(&fields[0]);
    let argv = format_string_array_for_exec(&fields[1]);
    let is_extended = matches!(fields.get(2), Some(PropertyValue::Array(_)));
    let flags = if is_extended {
        format_string_array(&fields[2])
    } else {
        fields.get(2).map(format_yes_no).unwrap_or("no").to_string()
    };
    // Both ExecStatus signatures use four timestamps followed by pid, code,
    // and status. The third field is either ignore_errors or flags; the
    // printed representation intentionally exposes only wall-clock start and
    // stop timestamps, matching systemctl's `print_property`.
    let start = format_timestamp(&fields[3]);
    let stop = format_timestamp(&fields[5]);
    let pid = generic(&fields[7]);
    let code = format_sigchld_code(&fields[8]);
    let status = format_exec_status_value(&fields[8], &fields[9]);

    if is_extended {
        format!(
            "{{ path={path} ; argv[]={argv} ; flags={flags} ; start_time=[{start}] ; stop_time=[{stop}] ; pid={pid} ; code={code} ; status={status} }}"
        )
    } else {
        format!(
            "{{ path={path} ; argv[]={argv} ; ignore_errors={flags} ; start_time=[{start}] ; stop_time=[{stop}] ; pid={pid} ; code={code} ; status={status} }}"
        )
    }
}

pub(super) fn format_string_array_for_exec(value: &PropertyValue) -> String {
    let PropertyValue::Array(array) = value else {
        return generic(value);
    };
    array.iter().map(generic).collect::<Vec<_>>().join(" ")
}

pub(super) fn format_exec_status_value(code: &PropertyValue, status: &PropertyValue) -> String {
    let status_text = generic(status);
    if value_as_i32(code) == Some(1) {
        return status_text;
    }

    let suffix = value_as_i32(status)
        .map(signal_name)
        .unwrap_or_else(|| status_text.clone());
    format!("{status_text}/{suffix}")
}

pub(super) fn signal_name(signal: i32) -> String {
    // Keep this in the same shape as systemd's signal_to_string(): the
    // symbolic name is used only for non-CLD_EXITED Exec status values.
    match signal {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        4 => "ILL",
        5 => "TRAP",
        6 => "ABRT",
        7 => "BUS",
        8 => "FPE",
        9 => "KILL",
        10 => "USR1",
        11 => "SEGV",
        12 => "USR2",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        16 => "STKFLT",
        17 => "CHLD",
        18 => "CONT",
        19 => "STOP",
        20 => "TSTP",
        21 => "TTIN",
        22 => "TTOU",
        23 => "URG",
        24 => "XCPU",
        25 => "XFSZ",
        26 => "VTALRM",
        27 => "PROF",
        28 => "WINCH",
        29 => "IO",
        30 => "PWR",
        31 => "SYS",
        _ => return signal.to_string(),
    }
    .to_string()
}

pub(super) fn format_sigchld_code(value: &PropertyValue) -> String {
    match value_as_i32(value) {
        Some(1) => "exited".to_string(),
        Some(2) => "killed".to_string(),
        Some(3) => "dumped".to_string(),
        Some(4) => "trapped".to_string(),
        Some(5) => "stopped".to_string(),
        Some(6) => "continued".to_string(),
        Some(0) => "(null)".to_string(),
        Some(code) => code.to_string(),
        None => generic(value),
    }
}
