//! NVIDIA GPU and driver detection logic for host passthrough.

pub mod cdi;
pub mod classify;
pub mod discovery;
pub mod lifecycle;
pub mod resolve;
pub mod state;

pub(crate) use discovery::get_nvidia_state_from;
pub use lifecycle::ensure_gpu_passthrough;
pub use state::{NvidiaState, NvidiaStateStore};
