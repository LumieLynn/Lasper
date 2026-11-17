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
