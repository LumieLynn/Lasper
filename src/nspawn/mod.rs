//! Core logic for interacting with systemd-nspawn and machinectl. 

pub mod config;
pub mod create;
pub mod deploy;
pub mod errors;
pub mod provider;
pub mod manager;
pub mod models;
pub mod network;
pub mod nvidia;
pub mod storage;

pub use models::ContainerEntry;
pub use models::ContainerState;

/// Severity level for status messages shown in the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel { Info, Success, Warn, Error }
