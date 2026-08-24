//! Transitional systemd-nspawn data models and shared errors.

pub mod errors;
pub mod models;

pub use models::{ContainerEntry, ContainerState, ImageEntry};
