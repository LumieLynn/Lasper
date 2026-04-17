//! NVIDIA GPU and driver detection logic for host passthrough.

pub mod state;
pub mod cdi;
pub mod resolve;
pub mod discovery;
pub mod lifecycle;

pub use state::NvidiaState;
pub use discovery::get_nvidia_state;
pub use lifecycle::ensure_gpu_passthrough;
