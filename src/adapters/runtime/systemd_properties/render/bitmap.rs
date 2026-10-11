//! Kernel masks with systemd semantic representations.

use super::generic::generic;
use super::scalar::value_as_u64;
use crate::domain::inspection::PropertyValue;

pub(super) fn format_capability_set(v: &PropertyValue) -> String {
    let Some(mask) = value_as_u64(v) else {
        return generic(v);
    };

    // systemd generates this table from linux/capability.h and lowercases the
    // names for the systemctl representation. Keep the table in capability
    // number order so the output follows capability_set_to_string().
    const NAMES: &[&str] = &[
        "cap_chown",
        "cap_dac_override",
        "cap_dac_read_search",
        "cap_fowner",
        "cap_fsetid",
        "cap_kill",
        "cap_setgid",
        "cap_setuid",
        "cap_setpcap",
        "cap_linux_immutable",
        "cap_net_bind_service",
        "cap_net_broadcast",
        "cap_net_admin",
        "cap_net_raw",
        "cap_ipc_lock",
        "cap_ipc_owner",
        "cap_sys_module",
        "cap_sys_rawio",
        "cap_sys_chroot",
        "cap_sys_ptrace",
        "cap_sys_pacct",
        "cap_sys_admin",
        "cap_sys_boot",
        "cap_sys_nice",
        "cap_sys_resource",
        "cap_sys_time",
        "cap_sys_tty_config",
        "cap_mknod",
        "cap_lease",
        "cap_audit_write",
        "cap_audit_control",
        "cap_setfcap",
        "cap_mac_override",
        "cap_mac_admin",
        "cap_syslog",
        "cap_wake_alarm",
        "cap_block_suspend",
        "cap_audit_read",
        "cap_perfmon",
        "cap_bpf",
        "cap_checkpoint_restore",
    ];

    let mut names = Vec::new();
    for bit in 0..64 {
        if mask & (1u64 << bit) == 0 {
            continue;
        }
        names.push(
            NAMES
                .get(bit)
                .copied()
                .map(str::to_string)
                .unwrap_or_else(|| format!("cap_{bit}")),
        );
    }
    if names.is_empty() {
        String::new()
    } else {
        names.join(" ")
    }
}

pub(super) fn format_cpu_set(v: &PropertyValue) -> String {
    let PropertyValue::Array(array) = v else {
        return generic(v);
    };
    if !array
        .iter()
        .all(|value| matches!(value, PropertyValue::U8(_)))
    {
        return generic(v);
    }

    let bytes: Vec<u8> = array
        .iter()
        .filter_map(|value| match value {
            PropertyValue::U8(byte) => Some(*byte),
            _ => None,
        })
        .collect();
    if bytes.is_empty() {
        return String::new();
    }

    let mut ranges = Vec::new();
    let mut range_start: Option<usize> = None;
    let mut previous = 0usize;
    for bit in 0..bytes.len() * 8 {
        let set = bytes[bit / 8] & (1u8 << (bit % 8)) != 0;
        if set {
            if range_start.is_none() {
                range_start = Some(bit);
            }
            previous = bit;
        } else if let Some(start) = range_start.take() {
            ranges.push(format_cpu_range(start, previous));
        }
    }
    if let Some(start) = range_start {
        ranges.push(format_cpu_range(start, previous));
    }

    if ranges.is_empty() {
        String::new()
    } else {
        ranges.join(" ")
    }
}

pub(super) fn format_cpu_range(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

pub(super) fn format_namespace_restriction(v: &PropertyValue) -> String {
    let Some(flags) = value_as_u64(v) else {
        return generic(v);
    };
    // systemd's NAMESPACE_FLAGS_ALL in v261 is the namespace clone flags
    // exposed by systemd.exec. Keep the special yes/no states and render the
    // partial set using the same short names as namespace_flags_to_string().
    let all = (libc::CLONE_NEWCGROUP
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWUSER
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWTIME) as u64;
    if flags & all == 0 {
        return "yes".to_string();
    }
    if flags & all == all {
        return "no".to_string();
    }
    const NAMES: &[(u64, &str)] = &[
        (libc::CLONE_NEWCGROUP as u64, "cgroup"),
        (libc::CLONE_NEWIPC as u64, "ipc"),
        (libc::CLONE_NEWNET as u64, "net"),
        (libc::CLONE_NEWNS as u64, "mnt"),
        (libc::CLONE_NEWPID as u64, "pid"),
        (libc::CLONE_NEWUSER as u64, "user"),
        (libc::CLONE_NEWUTS as u64, "uts"),
        (libc::CLONE_NEWTIME as u64, "time"),
    ];
    let names: Vec<&str> = NAMES
        .iter()
        .filter_map(|(bit, name)| (flags & bit != 0).then_some(*name))
        .collect();
    if names.is_empty() {
        generic(v)
    } else {
        names.join(" ")
    }
}

pub(super) fn format_mount_flags(v: &PropertyValue) -> String {
    let Some(flags) = value_as_u64(v) else {
        return generic(v);
    };
    // MS_SHARED/MS_SLAVE/MS_PRIVATE/MS_UNBINDABLE values are the only
    // propagation modes accepted by systemd's vtable.
    match flags {
        0 => String::new(),
        x if x == libc::MS_PRIVATE => "private".to_string(),
        x if x == libc::MS_SHARED => "shared".to_string(),
        x if x == libc::MS_SLAVE => "slave".to_string(),
        x if x == libc::MS_UNBINDABLE => "unbindable".to_string(),
        _ => generic(v),
    }
}
