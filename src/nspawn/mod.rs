//! Core logic for interacting with systemd-nspawn and machinectl.

pub mod adapters;
pub mod errors;
pub mod models;
pub mod ops;
pub mod platform;
pub mod sys;

pub use models::{ContainerEntry, ContainerState};
