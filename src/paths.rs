//! Host-layout path management.
//!
//! Packaging-sensitive roots live here so they can be overridden at compile
//! time. Protocol paths inside a guest and kernel ABI paths such as `/proc`,
//! `/sys`, and `/dev` stay with the domain adapter that owns them.
//!
//! ```sh
//! LASPER_MACHINES_DIR=/opt/containers cargo build
//! LASPER_STATE_DIR=/opt/lasper-state cargo build
//! LASPER_SYSTEMD_CONFIG_DIR=/etc/systemd cargo build
//! LASPER_SYSTEMD_RUNTIME_DIR=/run/systemd cargo build
//! ```

use std::path::PathBuf;

const MACHINES_DIR: &str = match option_env!("LASPER_MACHINES_DIR") {
    Some(v) => v,
    None => "/var/lib/machines",
};

const DEFAULT_STATE_ROOT: &str = match option_env!("LASPER_STATE_DIR") {
    Some(v) => v,
    None => "/var/lib/lasper",
};

const SYSTEMD_CONFIG_ROOT: &str = match option_env!("LASPER_SYSTEMD_CONFIG_DIR") {
    Some(v) => v,
    None => "/etc/systemd",
};

const SYSTEMD_RUNTIME_ROOT: &str = match option_env!("LASPER_SYSTEMD_RUNTIME_DIR") {
    Some(v) => v,
    None => "/run/systemd",
};

const CACHE_ROOT: &str = match option_env!("LASPER_CACHE_DIR") {
    Some(v) => v,
    None => "/var/cache/lasper",
};

const RUNTIME_ROOT: &str = match option_env!("LASPER_RUNTIME_DIR") {
    Some(v) => v,
    None => "/run/lasper",
};

const IMAGE_MOUNT_ROOT: &str = match option_env!("LASPER_IMAGE_MOUNT_DIR") {
    Some(v) => v,
    None => "/mnt",
};

/// Persistent systemd administrator configuration root.
pub fn systemd_config_root() -> PathBuf {
    PathBuf::from(SYSTEMD_CONFIG_ROOT)
}

/// Volatile systemd runtime state and configuration root.
pub fn systemd_runtime_root() -> PathBuf {
    PathBuf::from(SYSTEMD_RUNTIME_ROOT)
}

/// Administrator-owned systemd-nspawn settings directory.
pub fn nspawn_config_dir() -> PathBuf {
    systemd_config_root().join("nspawn")
}

/// Volatile systemd-nspawn settings directory.
pub fn nspawn_runtime_config_dir() -> PathBuf {
    systemd_runtime_root().join("nspawn")
}

/// Administrator-owned settings path for one validated machine name.
pub fn nspawn_config(name: &str) -> PathBuf {
    nspawn_config_dir().join(format!("{name}.nspawn"))
}

/// Persistent systemd system-unit directory.
pub fn systemd_system_unit_dir() -> PathBuf {
    systemd_config_root().join("system")
}

/// Volatile systemd system-unit directory.
pub fn systemd_runtime_unit_dir() -> PathBuf {
    systemd_runtime_root().join("system")
}

/// Base directory for systemd-machined containers.
pub fn machines_dir() -> PathBuf {
    PathBuf::from(MACHINES_DIR)
}

/// Runtime registration state maintained by systemd-machined.
pub fn runtime_machines_dir() -> PathBuf {
    systemd_runtime_root().join("machines")
}

/// Runtime registration state for one validated machine name.
pub fn runtime_machine_state(name: &str) -> PathBuf {
    runtime_machines_dir().join(name)
}

/// Root path for a container: `/var/lib/machines/<name>`
pub fn machine_root(name: &str) -> PathBuf {
    machines_dir().join(name)
}

/// Disk-image path for a container with an arbitrary extension:
/// `/var/lib/machines/<name>.<ext>`
pub fn machine_image(name: &str, ext: &str) -> PathBuf {
    machines_dir().join(format!("{}.{}", name, ext))
}

/// Raw disk-image path: `/var/lib/machines/<name>.raw`
pub fn machine_raw_image(name: &str) -> PathBuf {
    machine_image(name, "raw")
}

/// Stable mount point used while provisioning a managed disk image.
pub fn machine_image_mount(name: &str) -> PathBuf {
    PathBuf::from(IMAGE_MOUNT_ROOT).join(format!("lasper-{name}"))
}

/// Parent directory for short-lived mounts used to configure imported raw images.
pub fn rootfs_mounts_dir() -> PathBuf {
    PathBuf::from(CACHE_ROOT).join("mounts")
}

/// Trusted root for durable privileged state.
///
/// This is fixed at build time. In particular, the elevated daemon never
/// inherits a caller-controlled runtime environment override for this path.
pub fn trusted_state_root() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_ROOT)
}

/// Stable machine-name locks for configuration writers, independent of the
/// administrator file and its legacy sidecars. Cleared by the host at reboot.
pub(crate) fn nspawn_settings_locks_dir() -> PathBuf {
    PathBuf::from(RUNTIME_ROOT).join("locks/nspawn")
}

/// Log directory when running as root: `<trusted_state_root>/logs`
pub fn log_dir() -> PathBuf {
    trusted_state_root().join("logs")
}
