//! NVIDIA GPU and driver detection logic for host passthrough.

pub mod state;
pub mod cdi;
pub mod resolve;
pub mod discovery;
pub mod lifecycle;

pub use state::NvidiaState;
pub use discovery::{get_host_driver_version, get_nvidia_state};
pub use lifecycle::{cleanup_container_garbage, ensure_gpu_passthrough};
