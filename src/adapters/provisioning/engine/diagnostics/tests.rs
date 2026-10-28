use super::*;

#[test]
fn skipped_output_is_reported_when_the_view_catches_up() {
    let (events, mut receiver) = tokio::sync::mpsc::channel(2);
    let diagnostics = DeploymentDiagnostics::new(
        DeploymentId::from_u128(1),
        MachineName::new("test").unwrap(),
        events,
    );
    diagnostics.message("first");
    diagnostics.message("second");
    diagnostics.finish(&Err(DeploymentError::failed("source failed")));
    receiver.try_recv().unwrap();
    receiver.try_recv().unwrap();

    diagnostics.message("rollback finished");

    assert!(
        matches!(receiver.try_recv().unwrap(), DeploymentEvent::Line(notice)
        if notice.starts_with("1 deployment log messages omitted"))
    );
    assert!(
        matches!(receiver.try_recv().unwrap(), DeploymentEvent::Line(last)
        if last == "rollback finished")
    );
}

#[test]
fn deploy_stream_classifies_recoverable_bootstrap_diagnostics() {
    for message in [
        "W: Failure trying to run: test-dev-null",
        "test-dev-null: Permission denied",
        "E: bootstrap failed",
        "operation not permitted while probing target",
    ] {
        assert!(is_high_signal(message), "did not classify {message:?}");
    }
}

#[test]
fn deploy_stream_distinguishes_normal_output_from_warnings() {
    for message in [
        "I: Retrieving base-files",
        "I: Extracting base-passwd",
        "Download complete",
    ] {
        assert!(!is_high_signal(message), "misclassified {message:?}");
    }
}
