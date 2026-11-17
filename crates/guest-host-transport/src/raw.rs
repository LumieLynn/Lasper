//! Low-level connection, framing and worker inspection for protocol tests.
//!
//! These types expose request IDs, wire frames and descriptor roles. Callers
//! assemble the checks themselves; ordinary execution uses GuestClient and Session.

pub use crate::connection::BrokerConnection;
pub use crate::identity::{
    IdentityPeerCredentials, IdentityWorker, UnixIdentityChannel, WorkerProcess,
};
pub use crate::preparation::{PreparedIo, PreparedWorker};
pub use crate::readiness::{WorkerHello, WorkerReadiness, WorkerReadinessError};
pub use crate::session::BrokerSession;
pub use crate::stream::{HandshakeError, ReceivedControlFrame, UnixControlStream};
