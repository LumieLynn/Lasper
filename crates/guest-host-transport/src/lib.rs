#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper guest transport currently supports Linux only");

mod stream;

pub use stream::{ReceivedControlFrame, UnixControlStream};
