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
mod tests {
    use super::*;
    use crate::session::{GuardianError, WorkerFailure, WorkerStage};

    #[test]
    fn the_worker_operation_determines_the_failure_category() {
        assert_eq!(
            WorkerFailure::new(WorkerStage::Account, 2).class(),
            FailureClass::Configuration
        );
        assert_eq!(
            WorkerFailure::new(WorkerStage::Exec, 2).class(),
            FailureClass::Exec
        );
        assert_eq!(
            WorkerFailure::new(WorkerStage::UserIdentity, 1).class(),
            FailureClass::Permission
        );
    }

    #[test]
    fn bootstrap_expiry_and_bad_input_have_different_statuses() {
        assert_eq!(GuardianError::HelloExpired.class(), FailureClass::TimedOut);
        assert_eq!(
            GuardianError::StartupNotReleased.class(),
            FailureClass::Protocol
        );
        assert_eq!(
            GuardianError::BrokerDisconnected.class(),
            FailureClass::Unavailable
        );
    }
}
