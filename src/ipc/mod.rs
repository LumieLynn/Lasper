//! Private contracts shared by the unprivileged client and root daemon.
//!
//! This module owns only wire-level protocol and transport primitives. It
//! does not select host adapters, execute operations, or contain presentation
//! policy.

pub(crate) mod protocol;
pub(crate) mod transport;
