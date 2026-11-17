//! Checks identity challenges against the pidfd assigned in Ready.
//!
//! The channel binds replies to a nonce and preparation. WorkerProcess checks
//! process lifetime and credential mappings. PreparedWorker adds machine and
//! terminal checks; neither layer proves the guardian's assignment history.

mod channel;
mod id_map;
mod process;
mod worker;

pub use channel::{IdentityPeerCredentials, UnixIdentityChannel};
use id_map::IdMap;
pub use process::WorkerProcess;
pub use worker::{IdentityWorker, WorkerObservation};
