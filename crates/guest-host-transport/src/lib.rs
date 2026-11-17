//! Host connections and worker observations for the Lasper guest runtime.
//!
//! Acquire a MachineTarget, then use GuestClient to probe or prepare execution.
//! Prepare returns a Session control owner and independent SessionIo descriptors.
//! The background session driver owns protocol ordering, lease renewal and
//! cleanup observation. Protocol tests use the lower-level types in [`raw`].
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
//! membership. PreparedWorker checks target membership and PTY ownership before
//! delivering data descriptors. Before Start, the driver repeats identity and
//! target checks; it no longer owns the delivered PTY. Start acceptance does not
//! confirm exec success.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper guest transport currently supports Linux only");

mod client;
mod connection;
mod identity;
mod io;
mod preparation;
mod process;
pub mod raw;
mod readiness;
mod session;
mod stream;
mod target;

const SCM_PIDFD: libc::c_int = 4;

pub use connection::BrokerConnectError;
use connection::BrokerConnection;
pub use identity::WorkerObservation;
use identity::{IdentityWorker, UnixIdentityChannel, WorkerProcess};
use preparation::{PreparedIo, PreparedWorker};
use readiness::{WorkerHello, WorkerReadiness, WorkerReadinessError};
use session::BrokerSession;
pub use session::{
    BrokerSessionError, Completion, SessionPhase, SessionSnapshot, StartError, StartOutcome,
    StartSubmission,
};
pub use stream::NegotiatedHello;
use stream::{HandshakeError, ReceivedControlFrame, UnixControlStream};
pub use target::{MachineRegistration, MachineTarget, MachineTargetSource};

pub use client::{
    open_shell, ClientOptions, ClientTimeouts, GuestClient, PrepareCapacityError, PrepareError,
    Prepared, ProcessSpec, Program, RequestError, RuntimeInfo, Session, ShellOpenError,
    StartOptions, Stdio, DEFAULT_ENDPOINT,
};
pub use io::{PipeReader, PtyMaster, SessionIo};
pub use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestFailure,
    GuestFailureStage, TerminalSize,
};
