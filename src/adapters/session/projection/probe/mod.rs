//! Shared mechanics for fixed, read-only guest probes.
//!
//! Display resolvers own projection policy. This module owns path arguments,
//! guest filesystem observations, protocol framing, and bounded PTY collection.

mod protocol;
mod request;
mod socket;

#[cfg(test)]
mod tests;

pub(super) use protocol::ProbeProtocol;
pub(super) use request::{validate_absolute_path, ProbePathError};
pub(super) use socket::{SocketAccess, SocketObservation};

const GUEST_SCRIPT: &str = include_str!("guest.sh");

pub(super) fn guest_script(magic: &str, body: &str) -> String {
    format!("probe_magic='{magic}'\n{GUEST_SCRIPT}\n{body}")
}
