#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper guest transport currently supports Linux only");

mod identity;
mod stream;

const SCM_PIDFD: libc::c_int = 4;

pub use identity::{IdentityPeerCredentials, UnixIdentityChannel};
pub use stream::{ReceivedControlFrame, UnixControlStream};
