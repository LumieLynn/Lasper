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

/// Base directory for systemd-machined containers.
pub fn machines_dir() -> PathBuf {
    PathBuf::from(MACHINES_DIR)
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
