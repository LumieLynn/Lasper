//! Core logic for interacting with systemd-nspawn and machinectl.

pub mod ops;
pub mod adapters;
pub mod platform;
pub mod sys;
pub mod models;
pub mod errors;

pub use models::{ContainerEntry, ContainerState};
