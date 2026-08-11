//! Centralized path management.
//!
//! All hardcoded system paths live here so they can be overridden at compile time:
//!
//! ```sh
//! LASPER_MACHINES_DIR=/opt/containers cargo build
//! LASPER_STATE_DIR=/opt/lasper-state cargo build
//! ```
//!
//! At runtime, `LASPER_STATE_DIR` also overrides the state directory.

use std::path::PathBuf;

const MACHINES_DIR: &str = match option_env!("LASPER_MACHINES_DIR") {
    Some(v) => v,
    None => "/var/lib/machines",
};

const DEFAULT_STATE_DIR: &str = match option_env!("LASPER_STATE_DIR") {
    Some(v) => v,
    None => "/var/lib/lasper",
};

const SYSTEMD_RUNTIME_MACHINES_DIR: &str = "/run/systemd/machines";

/// Base directory for systemd-machined containers.
pub fn machines_dir() -> PathBuf {
    PathBuf::from(MACHINES_DIR)
}

/// Runtime registration state maintained by systemd-machined.
pub fn runtime_machines_dir() -> PathBuf {
    PathBuf::from(SYSTEMD_RUNTIME_MACHINES_DIR)
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
    PathBuf::from("/mnt").join(format!("lasper-{}", name))
}

/// Parent directory for short-lived mounts used to configure imported raw images.
pub fn rootfs_mounts_dir() -> PathBuf {
    PathBuf::from("/var/cache/lasper/mounts")
}

/// State directory (NVIDIA passthrough, etc.).
/// `LASPER_STATE_DIR` env var overrides at runtime;
/// otherwise the compile-time default (`/var/lib/lasper/states`).
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LASPER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(DEFAULT_STATE_DIR).join("states")
}

/// Per-container state file: `<state_dir>/<name>.json`
pub fn state_file(name: &str) -> PathBuf {
    state_dir().join(format!("{}.json", name))
}

/// Log directory when running as root: `<DEFAULT_STATE_DIR>/logs`
pub fn log_dir() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_DIR).join("logs")
}
