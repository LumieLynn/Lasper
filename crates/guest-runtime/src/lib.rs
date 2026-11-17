#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! Guest-side process supervision without libc or a heap allocator.
//!
//! The wrapper preserves the init or payload process across exec. Its broker
//! forks a guardian for each connection, and that guardian owns worker execution
//! and descendant cleanup. Large buffers use anonymous mappings so the same
//! execution path can run on the static ELF's explicitly budgeted stack.

#[cfg(not(target_os = "linux"))]
compile_error!("the Lasper guest runtime is Linux-only");

use lasper_guest_protocol::{Architecture, RuntimeIdentity};

pub mod broker;
#[doc(hidden)]
pub mod diagnostic;
pub mod failure;
pub mod identity;
mod linux;
pub use linux::signals::SupervisorSignal;
mod memory;
pub mod paths;
pub mod policy;
pub mod readiness;
pub mod session;
mod socket;
mod syscall;
pub mod transport;
pub mod wrapper;

#[cfg(target_arch = "x86_64")]
const RUNTIME_ARCHITECTURE: Architecture = Architecture::X86_64;

#[cfg(target_arch = "aarch64")]
const RUNTIME_ARCHITECTURE: Architecture = Architecture::Aarch64;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the Lasper guest runtime does not support this architecture");

pub const RUNTIME_IDENTITY: RuntimeIdentity = RuntimeIdentity::current(RUNTIME_ARCHITECTURE);

pub fn identity_frame() -> [u8; lasper_guest_protocol::IDENTITY_FRAME_BYTES] {
    RUNTIME_IDENTITY.encode()
}

#[cfg(test)]
#[path = "../tests/entry/identity.rs"]
mod tests;
