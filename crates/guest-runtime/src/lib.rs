#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("the Lasper guest runtime is Linux-only");

use lasper_guest_protocol::{Architecture, RuntimeIdentity};

pub mod identity;
pub mod readiness;
mod socket;
mod syscall;
pub mod transport;

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
mod tests {
    use super::*;

    #[test]
    fn advertised_identity_is_the_shared_protocol_identity() {
        let frame = identity_frame();

        assert_eq!(
            lasper_guest_protocol::RuntimeIdentity::decode(&frame),
            Ok(RUNTIME_IDENTITY)
        );
    }
}
