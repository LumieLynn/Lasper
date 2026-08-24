//! Elevated daemon — a long-running root child process that executes closed,
//! typed privileged operations on behalf of the unprivileged TUI.
//!
//! The daemon serves JSON-RPC 2.0 over a mutually authenticated Unix stream
//! (one JSON object per line). The daemon is spawned via
//! `sudo <self> --daemon` before the terminal enters raw mode, so the sudo
//! password prompt appears on the clean terminal.
//!
//! ## Architecture
//!
//! A dedicated I/O task serializes all RPC traffic through the authenticated
//! Unix stream. Callers send `(request, oneshot::Sender)` tuples over an mpsc
//! channel; the I/O task writes the request, reads the response, and fulfills
//! the oneshot. This avoids locking issues around async socket I/O.
//!
//! ## FD passing
//!
//! Long-running commands (journalctl -f, terminal attachment) are spawned by
//! the daemon as root. The daemon passes the resulting fd back to the parent
//! over a Unix domain socket using the [`sendfd`] crate. The socket is scoped
//! to a private per-session directory, owned by the launching user, and each
//! connection must match both the TUI's PID/UID via `SO_PEERCRED` and a random
//! session token negotiated after the control connection's peer credentials
//! have been authenticated.

mod command;
pub(crate) mod deployment_protocol;
mod deployment_server;
mod dispatch;
mod job;
mod logging;
mod process_state;
pub(crate) mod protocol;
mod query;
mod server;
pub(crate) mod session_protocol;
mod session_server;
pub(crate) mod transport;

pub use server::daemon_main;

#[cfg(test)]
mod tests;
