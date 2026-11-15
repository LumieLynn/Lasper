//! Host connections and worker observations for the Lasper guest runtime.
//!
//! The host trusts its kernel, procfs view and machine registration source. It
//! pins the machine and peer processes, checks namespace/root/cgroup membership,
//! and binds identity replies to the specific worker pidfd supplied in Ready.
//! Numeric guest IDs must map to the sender's observed kernel credentials.
//!
//! The guest runtime is also trusted. The host cannot independently establish
//! that the guardian created this worker for this Prepare or delivered Start to
//! it. Account-name resolution, command execution and descendant-cleanup reports
//! rely on that runtime behaving correctly. These checks do not attest execution
//! against a compromised guardian or guest administrator.
//!
//! Low-level identity observations establish sender credentials, not machine
//! membership. PreparedWorker adds target and PTY checks; BrokerSession repeats
//! those checks before Start. Acceptance of Start does not confirm exec success.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper guest transport currently supports Linux only");

mod connection;
mod identity;
mod preparation;
mod process;
mod readiness;
mod session;
mod stream;
mod target;

const SCM_PIDFD: libc::c_int = 4;

pub use connection::{BrokerConnectError, BrokerConnection};
pub use identity::{
    IdentityPeerCredentials, IdentityWorker, UnixIdentityChannel, WorkerObservation, WorkerProcess,
};
pub use preparation::PreparedWorker;
pub use readiness::{WorkerHello, WorkerReadiness, WorkerReadinessError};
pub use session::{
    BrokerSession, BrokerSessionError, SessionPhase, SessionSnapshot, SessionStartOptions,
    StartError, StartOutcome,
};
pub use session::{CapturedStream, SessionExit, SessionOutput};
pub use stream::{HandshakeError, NegotiatedHello, ReceivedControlFrame, UnixControlStream};
pub use target::{MachineRegistration, MachineTarget, MachineTargetSource};
