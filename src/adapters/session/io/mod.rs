//! I/O ownership after native session creation.
//!
//! PTY bridges deliver application terminal handles; terminal bridges forward
//! those handles to the caller's terminal or a desktop launcher stream.

mod pty;
mod terminal;

pub(crate) use pty::MachinePty;
pub(super) use pty::{spawn_direct_terminal, spawn_fd_terminal, spawn_machine_terminal};
pub(crate) use terminal::{
    inherited_terminal_size, launcher_terminal_size, run_inherited_terminal, run_launcher_terminal,
};
