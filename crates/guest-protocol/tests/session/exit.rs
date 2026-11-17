use super::support::{preparation, protocol, request, resized};
use lasper_guest_protocol::{
    CleanupStatus, ControlFrameHeader, ExecutionKind, ExecutionStatus, ExitFrame, ExitReason,
    ExitReport, ExitStatus, GuestFailure, GuestFailureStage, MessageKind, PreparationId,
    SessionFrameError, SessionPayloadError,
};
fn report(cleanup: CleanupStatus) -> ExitReport {
    ExitReport::new(
        ExecutionStatus::Started,
        Some(ExitStatus::Exited(0)),
        cleanup,
        ExitReason::WorkerExited,
    )
    .unwrap()
}
#[test]
fn exit_facts_preserve_exec_failures_and_descendant_cleanup() {
    let intermediate = report(CleanupStatus::InProgress);
    assert!(!intermediate.is_final());
    assert_eq!(intermediate.exit(), Some(ExitStatus::Exited(0)));
    assert!(report(CleanupStatus::Complete).is_final());
    assert!(ExitReport::new(
        ExecutionStatus::Unknown,
        Some(ExitStatus::Signaled(9)),
        CleanupStatus::Complete,
        ExitReason::StartOutcomeUnknown
    )
    .is_ok());
    for (execution, exit, cleanup, reason) in [
        (
            ExecutionStatus::Failed { errno: 0 },
            Some(ExitStatus::Exited(1)),
            CleanupStatus::Complete,
            ExitReason::ExecFailed,
        ),
        (
            ExecutionStatus::Started,
            Some(ExitStatus::Signaled(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        ),
        (
            ExecutionStatus::Started,
            None,
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        ),
        (
            ExecutionStatus::Starting,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::InProgress,
            ExitReason::WorkerExited,
        ),
        (
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(1)),
            CleanupStatus::Complete,
            ExitReason::ExecFailed,
        ),
    ] {
        assert_eq!(
            ExitReport::new(execution, exit, cleanup, reason),
            Err(SessionPayloadError::InvalidResponse)
        );
    }
}

#[test]
fn exit_rejects_unknown_fields_in_execution_and_cleanup() {
    let request = request(ExecutionKind::Piped);
    let report = ExitReport::new(
        ExecutionStatus::Failed { errno: 2 },
        Some(ExitStatus::Exited(1)),
        CleanupStatus::Pending { errno: Some(5) },
        ExitReason::ExecFailed,
    )
    .unwrap();
    let frame = ExitFrame::new(&request, preparation(), report).unwrap();
    let mut bytes = [0; 1024];
    let (header, length) = protocol().encode_exit(&frame, &mut bytes).unwrap();
    let payload = std::str::from_utf8(&bytes[..length]).unwrap();
    for (original, replacement) in [
        ("\"errno\":2", "\"errno\":2,\"unknown\":true"),
        ("\"errno\":5", "\"errno\":5,\"unknown\":true"),
    ] {
        let invalid = payload.replace(original, replacement);
        assert_ne!(invalid, payload);
        assert!(protocol()
            .decode_exit(
                &request,
                preparation(),
                resized(header, invalid.len()),
                invalid.as_bytes(),
                0,
            )
            .is_err());
    }
}

#[test]
fn exit_decode_checks_the_envelope_tokens_and_semantics() {
    let request = request(ExecutionKind::Terminal);
    let frame = ExitFrame::new(&request, preparation(), report(CleanupStatus::Complete)).unwrap();
    let mut bytes = [0; 1024];
    let (header, length) = protocol().encode_exit(&frame, &mut bytes).unwrap();
    let payload = &bytes[..length];
    assert!(protocol()
        .decode_exit(&request, PreparationId::new([1; 16]), header, payload, 0)
        .is_err());
    assert!(protocol()
        .decode_exit(&request, preparation(), header, payload, 1)
        .is_err());
    let wrong = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::Exit,
        8,
        length as u32,
        0,
    )
    .unwrap();
    assert!(matches!(
        protocol().decode_exit(&request, preparation(), wrong, payload, 0),
        Err(SessionFrameError::MismatchedRequestId { .. })
    ));
    let changed = std::str::from_utf8(payload)
        .unwrap()
        .replace("\"exited\":0", "\"signaled\":0");
    assert!(matches!(
        protocol().decode_exit(
            &request,
            preparation(),
            resized(header, changed.len()),
            changed.as_bytes(),
            0
        ),
        Err(SessionFrameError::Payload(
            SessionPayloadError::InvalidResponse
        ))
    ));
}

#[test]
fn worker_failure_details_round_trip_and_cannot_replace_the_primary_reason() {
    let request = request(ExecutionKind::Terminal);
    let failure = GuestFailure::new(GuestFailureStage::Exec, 2).unwrap();
    let report = ExitReport::new(
        ExecutionStatus::Failed { errno: 2 },
        Some(ExitStatus::Exited(126)),
        CleanupStatus::Complete,
        ExitReason::ExecFailed,
    )
    .unwrap()
    .with_failure(Some(failure))
    .unwrap();
    let frame = ExitFrame::new(&request, preparation(), report).unwrap();
    let mut bytes = [0; 2048];
    let (header, length) = protocol().encode_exit(&frame, &mut bytes).unwrap();
    assert_eq!(
        protocol()
            .decode_exit(&request, preparation(), header, &bytes[..length], 0)
            .unwrap()
            .report(),
        report
    );
    let text = std::str::from_utf8(&bytes[..length]).unwrap();
    for invalid in [
        text.replace("\"stage\":\"exec\"", "\"stage\":\"account\""),
        text.replace(
            "\"stage\":\"exec\",\"errno\":2",
            "\"stage\":\"exec\",\"errno\":13",
        ),
        text.replace("\"exec_failed\"", "\"cancelled\""),
        text.replace("\"errno\":2", "\"errno\":4096"),
    ] {
        assert_ne!(invalid, text);
        assert!(
            protocol()
                .decode_exit(
                    &request,
                    preparation(),
                    resized(header, invalid.len()),
                    invalid.as_bytes(),
                    0
                )
                .is_err(),
            "accepted {invalid}"
        );
    }
    for errno in [-1, 0, 4096] {
        assert!(GuestFailure::new(GuestFailureStage::Account, errno).is_err());
    }
}

#[test]
fn ordinary_exit_preserves_known_status_without_requiring_exec_confirmation() {
    assert_eq!(
        report(CleanupStatus::Complete).command_exit(),
        Some(ExitStatus::Exited(0))
    );
    assert_eq!(report(CleanupStatus::InProgress).command_exit(), None);
    for reason in [
        ExitReason::Cancelled,
        ExitReason::ExecutionTimedOut,
        ExitReason::ClockFailed,
    ] {
        assert_eq!(
            ExitReport::new(
                ExecutionStatus::Started,
                Some(ExitStatus::Exited(0)),
                CleanupStatus::Complete,
                reason
            )
            .unwrap()
            .command_exit(),
            None
        );
    }
    assert_eq!(
        ExitReport::new(
            ExecutionStatus::Unknown,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited
        )
        .unwrap()
        .command_exit(),
        Some(ExitStatus::Exited(0))
    );
}

#[test]
fn exit_is_status_only_for_both_io_modes() {
    for mode in [ExecutionKind::Terminal, ExecutionKind::Piped] {
        let request = request(mode);
        let frame =
            ExitFrame::new(&request, preparation(), report(CleanupStatus::Complete)).unwrap();
        let mut bytes = [0; 1024];
        let (header, n) = protocol().encode_exit(&frame, &mut bytes).unwrap();
        let decoded = protocol()
            .decode_exit(&request, preparation(), header, &bytes[..n], 0)
            .unwrap();
        assert_eq!(decoded.report(), report(CleanupStatus::Complete));
        let text = std::str::from_utf8(&bytes[..n]).unwrap();
        assert!(!text.contains("output"));
        let unknown = text.trim_end_matches('}').to_owned() + ",\"output\":null}";
        assert!(protocol()
            .decode_exit(
                &request,
                preparation(),
                resized(header, unknown.len()),
                unknown.as_bytes(),
                0
            )
            .is_err());
    }
}
