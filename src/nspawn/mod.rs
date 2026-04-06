//! Core logic for interacting with systemd-nspawn and machinectl.

pub mod core;
pub mod config;
pub mod rootfs;
pub mod hw;
pub mod utils;

pub mod deploy;
pub mod errors;
pub mod models;

pub use models::ContainerEntry;
pub use models::ContainerState;

/// Severity level for status messages shown in the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warn,
    Error,
}
