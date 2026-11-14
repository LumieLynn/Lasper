//! The client resolves the broker socket inside the guest root and uses host
//! paths for its own terminal, procfs and D-Bus queries.

/// This default must match the runtime's directory and socket name.
pub const BROKER_SOCKET: &str = "/var/opt/lasper/guest-exec.sock";
/// Interactive I/O uses the controlling terminal even when stdio is redirected.
pub const CONTROLLING_TERMINAL: &str = "/dev/tty";
pub const PROC_ROOT: &str = "/proc";
pub const PROC_SELF_FD: &str = "/proc/self/fd";

// These object paths address managers on the host system bus.
pub const MACHINE_MANAGER: &str = "/org/freedesktop/machine1";
pub const SYSTEM_MANAGER: &str = "/org/freedesktop/systemd1";
