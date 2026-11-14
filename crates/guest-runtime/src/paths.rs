//! All paths in this module are interpreted in the guest filesystem.

use core::ffi::CStr;

/// Stores the broker socket and lock outside `/run`, which init may mount over.
/// The directory must be owned by guest root and have mode 0700.
pub const RUNTIME_DIRECTORY: &CStr = c"/var/opt/lasper";

/// The automatic wrapper executes this path, which must remain the real guest init.
pub const ORIGINAL_INIT: &CStr = c"/sbin/init";

/// nspawn tries these paths before `/sbin/init`. The wrapper may be bound only
/// to an absent candidate so it does not hide an installed init.
pub const INIT_CANDIDATES: [&CStr; 2] = [c"/lib/systemd/systemd", c"/usr/lib/systemd/systemd"];

pub(crate) const BROKER_SOCKET_NAME: &CStr = c"guest-exec.sock";
/// The file survives broker restarts so competing instances lock the same inode.
pub(crate) const BROKER_LOCK_NAME: &CStr = c"broker.lock";
pub(crate) const PROC_ROOT: &CStr = c"/proc";
pub(crate) const ROOT_DIRECTORY: &CStr = c"/";
pub(crate) const PROC_SELF_EXE: &CStr = c"/proc/self/exe";
pub(crate) const PROC_SELF_FD: &CStr = c"/proc/self/fd";
pub(crate) const NULL_DEVICE: &CStr = c"/dev/null";
pub(crate) const PTY_MULTIPLEXER: &CStr = c"/dev/ptmx";
pub(crate) const PASSWD: &CStr = c"/etc/passwd";
pub(crate) const GROUP: &CStr = c"/etc/group";
/// Command lookup uses guest directories independently of the host's PATH.
pub(crate) const EXEC_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
