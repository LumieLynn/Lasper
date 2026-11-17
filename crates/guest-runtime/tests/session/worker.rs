use super::*;

#[test]
fn identity_failure_preserves_syscall_errors_and_distinguishes_bad_packets_from_eof() {
    for (error, errno) in [
        (IdentityTransportError::Syscall(13), 13),
        (IdentityTransportError::UnexpectedEof, EPIPE),
        (IdentityTransportError::PreparationMismatch, EINVAL),
        (IdentityTransportError::Truncated, EINVAL),
    ] {
        assert_eq!(
            identity_failure(error),
            WorkerFailure::new(WorkerStage::Identity, errno)
        );
    }
}
