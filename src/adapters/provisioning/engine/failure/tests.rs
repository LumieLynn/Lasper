use super::*;
use crate::adapters::process::unconfirmed_process_exit;
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::trusted_state::TrustedStateError;

#[test]
fn confirmed_timeout_does_not_retain_claims_after_complete_rollback() {
    let cause = OperationFailure::Io(
        "systemd-nspawn".into(),
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "helper exceeded its deadline; termination was confirmed",
        ),
    );
    let error = DeploymentFailure::new(cause, Vec::new()).into_application_error();
    assert!(!error.requires_reconciliation());
    assert!(!error.retains_resource_claim());
}

#[test]
fn unconfirmed_exit_survives_adapter_error_context_and_retains_claims() {
    let io = unconfirmed_process_exit("helper", std::io::Error::other("wait failed"));
    let result: Result<(), TrustedStateError> =
        Err(TrustedStateError::Io("guest rootfs".into(), io));
    let cause = result.host_context("configure guest").unwrap_err();
    assert!(cause.outcome_is_unconfirmed());
    assert!(cause.to_string().contains("configure guest"));
    assert!(cause.to_string().contains("wait failed"));
    let error = DeploymentFailure::new(cause, Vec::new()).into_application_error();
    assert!(error.requires_reconciliation());
    assert!(error.retains_resource_claim());
}

#[test]
fn cleanup_failure_preserves_cancellation_and_requires_reconciliation() {
    let error = DeploymentFailure::new(
        OperationFailure::Cancelled,
        vec!["storage unmount failed".into()],
    )
    .into_application_error();
    assert!(error.to_string().contains("cancelled"));
    assert!(error.to_string().contains("storage unmount failed"));
    assert!(error.requires_reconciliation());
    let error =
        DeploymentFailure::new(OperationFailure::Cancelled, Vec::new()).into_application_error();
    assert!(error.is_cancelled());
    assert!(!error.retains_resource_claim());
}

#[test]
fn native_operation_uncertainty_is_not_lost_at_the_deployment_boundary() {
    let cause = OperationFailure::from(SystemOperationError::OutcomeUnknown(
        "clone completion was not observed".into(),
    ));
    assert!(cause.outcome_is_unconfirmed());
    let error = DeploymentFailure::new(cause, Vec::new()).into_application_error();
    assert!(error.retains_resource_claim());
}
