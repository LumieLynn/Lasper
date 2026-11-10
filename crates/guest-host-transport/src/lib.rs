#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper guest transport currently supports Linux only");

mod connection;
mod identity;
mod preparation;
mod process;
mod readiness;
mod stream;
mod target;

const SCM_PIDFD: libc::c_int = 4;

pub use connection::{BrokerConnectError, BrokerConnection};
pub use identity::{
    IdentityPeerCredentials, IdentityWorker, UnixIdentityChannel, WorkerObservation, WorkerProcess,
};
pub use preparation::PreparedWorker;
pub use readiness::{WorkerHello, WorkerReadiness, WorkerReadinessError};
pub use stream::{ReceivedControlFrame, UnixControlStream};
pub use target::{MachineRegistration, MachineTarget, MachineTargetSource};
