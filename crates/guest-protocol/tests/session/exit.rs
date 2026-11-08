use lasper_guest_protocol::{
    CapturedOutput, CleanupStatus, ControlFrameHeader, ControlLimits, ExecutionKind,
    ExecutionStatus, ExitFrame, ExitReason, ExitReport, ExitStatus, MessageKind, OutputStream,
    PayloadCodecError, PreparationId, SessionFrameError, SessionPayloadError,
    CONTROL_FRAME_HEADER_BYTES, MAX_CAPTURED_OUTPUT_BYTES, MIN_HELPER_CONTROL_FRAME_BYTES,
};

use super::support::{preparation, request, resized};

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
fn binary_output_round_trips_without_a_second_encoding_buffer() {
    let request = request(ExecutionKind::Bounded);
    let stdout: Vec<u8> = (0..=255).cycle().take(8191).collect();
    let stderr = [0xff, 0, b'\n', b'\r', b'"', b'\\'];
    let output = CapturedOutput::new(
        OutputStream::new(&stdout, true, true).unwrap(),
        OutputStream::new(&stderr, false, true).unwrap(),
    )
    .unwrap();
    let frame = ExitFrame::new(
        &request,
        preparation(),
        report(CleanupStatus::Complete),
        Some(output),
    )
    .unwrap();
    let mut bytes = vec![0; 16384];
    let (header, length) = frame.encode_parts(&mut bytes).unwrap();
    let decoded =
        ExitFrame::decode_for(&request, preparation(), header, &bytes[..length], 0).unwrap();
    let output = decoded.output().unwrap();
    let mut actual = vec![0; stdout.len()];
    output.stdout().write_into(&mut actual).unwrap();
    assert_eq!(actual, stdout);
    assert!(output.stdout().truncated());
    assert_eq!(
        output.stderr().write_into(&mut actual).unwrap(),
        stderr.len()
    );
    assert_eq!(&actual[..stderr.len()], &stderr);
    assert_eq!(
        output.stdout().write_into(&mut []),
        Err(PayloadCodecError::OutputTooSmall)
    );
    let mut encoded_again = vec![0; bytes.len()];
    let (_, second_length) = decoded.encode_parts(&mut encoded_again).unwrap();
    assert_eq!(&encoded_again[..second_length], &bytes[..length]);
}

#[test]
fn base64_is_validated_after_json_unescaping() {
    let request = request(ExecutionKind::Bounded);
    let output = CapturedOutput::new(
        OutputStream::new(b"\xff", false, true).unwrap(),
        OutputStream::new(b"", false, true).unwrap(),
    )
    .unwrap();
    let frame = ExitFrame::new(
        &request,
        preparation(),
        report(CleanupStatus::Complete),
        Some(output),
    )
    .unwrap();
    let mut bytes = [0; 1024];
    let (header, length) = frame.encode_parts(&mut bytes).unwrap();
    let text = std::str::from_utf8(&bytes[..length]).unwrap();
    let escaped = text.replace("/w==", r"\u002fw==");
    let decoded = ExitFrame::decode_for(
        &request,
        preparation(),
        resized(header, escaped.len()),
        escaped.as_bytes(),
        0,
    )
    .unwrap();
    let mut actual = [0];
    decoded
        .output()
        .unwrap()
        .stdout()
        .write_into(&mut actual)
        .unwrap();
    assert_eq!(actual, [255]);
    for invalid in ["/x==", "/w=", "/w==AAAA", "=w==", "/w?=", "/w=\""] {
        let changed = text.replace("/w==", invalid);
        assert!(
            ExitFrame::decode_for(
                &request,
                preparation(),
                resized(header, changed.len()),
                changed.as_bytes(),
                0
            )
            .is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn capture_budget_keeps_exit_facts_within_the_negotiated_frame() {
    let request = request(ExecutionKind::Bounded);
    for maximum in [MIN_HELPER_CONTROL_FRAME_BYTES, 4097, 8192, 65536, 1048576] {
        let limits = ControlLimits::new(maximum, 3).unwrap();
        let budget = CapturedOutput::budget(limits);
        assert!(budget <= MAX_CAPTURED_OUTPUT_BYTES);
        let bytes = vec![255; budget];
        let split = usize::from(budget != 0);
        let output = CapturedOutput::new(
            OutputStream::new(&bytes[..split], true, true).unwrap(),
            OutputStream::new(&bytes[split..], true, false).unwrap(),
        )
        .unwrap();
        let report = ExitReport::new(
            ExecutionStatus::Failed { errno: 4095 },
            Some(ExitStatus::Signaled(64)),
            CleanupStatus::Pending { errno: Some(4095) },
            ExitReason::ExecFailed,
        )
        .unwrap();
        let frame = ExitFrame::new(&request, preparation(), report, Some(output)).unwrap();
        let mut payload = vec![0; maximum as usize - CONTROL_FRAME_HEADER_BYTES];
        let (_, length) = frame.encode_parts(&mut payload).unwrap();
        assert!(length + CONTROL_FRAME_HEADER_BYTES <= maximum as usize);
        let header =
            ControlFrameHeader::new(MessageKind::Exit, request.request_id(), length as u32, 0)
                .unwrap();
        assert!(
            ExitFrame::decode_for(&request, preparation(), header, &payload[..length], 0).is_ok()
        );
    }
    let bytes = vec![0; MAX_CAPTURED_OUTPUT_BYTES];
    assert!(CapturedOutput::new(
        OutputStream::new(&bytes, false, true).unwrap(),
        OutputStream::new(b"x", false, true).unwrap()
    )
    .is_err());
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
fn output_presence_and_completeness_follow_the_execution_mode() {
    let bounded = request(ExecutionKind::Bounded);
    let interactive = request(ExecutionKind::Interactive);
    let open = CapturedOutput::new(
        OutputStream::new(b"", false, false).unwrap(),
        OutputStream::new(b"", false, true).unwrap(),
    )
    .unwrap();
    assert!(ExitFrame::new(
        &interactive,
        preparation(),
        report(CleanupStatus::Complete),
        None
    )
    .is_ok());
    assert!(ExitFrame::new(
        &interactive,
        preparation(),
        report(CleanupStatus::Complete),
        Some(open)
    )
    .is_err());
    assert!(ExitFrame::new(
        &bounded,
        preparation(),
        report(CleanupStatus::Complete),
        None
    )
    .is_err());
    assert!(ExitFrame::new(
        &bounded,
        preparation(),
        report(CleanupStatus::InProgress),
        Some(open)
    )
    .is_ok());
    assert!(ExitFrame::new(
        &bounded,
        preparation(),
        report(CleanupStatus::Complete),
        Some(open)
    )
    .is_err());
    let abandoned = CapturedOutput::new(
        OutputStream::new(b"", true, false).unwrap(),
        OutputStream::new(b"", false, true).unwrap(),
    )
    .unwrap();
    assert!(ExitFrame::new(
        &bounded,
        preparation(),
        report(CleanupStatus::Complete),
        Some(abandoned)
    )
    .is_ok());
}

#[test]
fn exit_decode_checks_the_envelope_tokens_and_semantics() {
    let request = request(ExecutionKind::Interactive);
    let frame = ExitFrame::new(
        &request,
        preparation(),
        report(CleanupStatus::Complete),
        None,
    )
    .unwrap();
    let mut bytes = [0; 1024];
    let (header, length) = frame.encode_parts(&mut bytes).unwrap();
    let payload = &bytes[..length];
    assert!(
        ExitFrame::decode_for(&request, PreparationId::new([1; 16]), header, payload, 0).is_err()
    );
    assert!(ExitFrame::decode_for(&request, preparation(), header, payload, 1).is_err());
    let wrong = ControlFrameHeader::new(MessageKind::Exit, 8, length as u32, 0).unwrap();
    assert!(matches!(
        ExitFrame::decode_for(&request, preparation(), wrong, payload, 0),
        Err(SessionFrameError::MismatchedRequestId { .. })
    ));
    let changed = std::str::from_utf8(payload)
        .unwrap()
        .replace("\"exited\":0", "\"signaled\":0");
    assert!(matches!(
        ExitFrame::decode_for(
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
fn decode_rejects_output_that_exceeds_the_combined_capture_limit() {
    let request = request(ExecutionKind::Bounded);
    let bytes = vec![0; MAX_CAPTURED_OUTPUT_BYTES];
    let output = CapturedOutput::new(
        OutputStream::new(&bytes, false, true).unwrap(),
        OutputStream::new(b"", false, true).unwrap(),
    )
    .unwrap();
    let frame = ExitFrame::new(
        &request,
        preparation(),
        report(CleanupStatus::Complete),
        Some(output),
    )
    .unwrap();
    let mut payload = vec![0; 96 * 1024];
    let (header, length) = frame.encode_parts(&mut payload).unwrap();
    let changed = std::str::from_utf8(&payload[..length])
        .unwrap()
        .replace("\"base64\":\"\"", "\"base64\":\"AA==\"");
    assert!(matches!(
        ExitFrame::decode_for(
            &request,
            preparation(),
            resized(header, changed.len()),
            changed.as_bytes(),
            0
        ),
        Err(SessionFrameError::Payload(SessionPayloadError::Payload(
            PayloadCodecError::PayloadTooLarge
        )))
    ));
}
