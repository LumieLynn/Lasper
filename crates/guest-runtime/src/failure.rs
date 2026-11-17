//! Exit categories for runtime failures. Executed programs keep their own status.

use crate::linux::uapi::errno::{EACCES, EPERM, ETIMEDOUT};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum FailureClass {
    Usage = 64,
    Protocol = 65,
    Unavailable = 69,
    Internal = 70,
    System = 71,
    Io = 74,
    TimedOut = 75,
    Permission = 77,
    Configuration = 78,
    Exec = 126,
    Artifact = 127,
}

impl FailureClass {
    pub const fn exit_code(self) -> i32 {
        self as i32
    }

    pub(crate) const fn system(errno: i32) -> Self {
        match errno {
            EACCES | EPERM => Self::Permission,
            ETIMEDOUT => Self::TimedOut,
            _ => Self::System,
        }
    }
}

#[cfg(test)]
#[path = "../tests/failure/mod.rs"]
mod tests;
