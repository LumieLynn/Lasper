use lasper_guest_protocol::*;

#[test]
fn prepare_environment_requires_valid_json_string_encoding() {
    for byte in 0..0x20u8 {
        let mut payload = br#"{"user":"root","execution":"piped","process":"login_shell","environment":[{"name":"VALUE","value":"a"#.to_vec();
        payload.push(byte);
        payload.extend_from_slice(br#"b"}]}"#);
        assert!(
            protocol()
                .decode_prepare(&payload, &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
                .is_err(),
            "unescaped byte {byte:#x} was accepted"
        );
    }
}

#[test]
fn prepare_strings_reject_malformed_escapes_and_decode_surrogate_pairs() {
    for invalid in [
        r"\q",
        r"\u+041",
        r"\uD800",
        r"\uDC00",
        r"\uD800\u0041",
        r"\uD800\uD800",
    ] {
        for value in [
            format!(
                r#"{{"user":"root","execution":"piped","process":{{"command":["/bin/echo","{invalid}"]}},"environment":[]}}"#
            ),
            format!(
                r#"{{"user":"root","execution":"piped","process":"login_shell","environment":[{{"name":"VALUE","value":"{invalid}"}}]}}"#
            ),
        ] {
            assert!(
                protocol()
                    .decode_prepare(value.as_bytes(), &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
                    .is_err(),
                "{value}"
            );
        }
    }
    let payload = br#"{"user":"root","execution":"piped","process":{"command":["/bin/echo","\uD800\uDC00\uDBFF\uDFFF"]},"environment":[]}"#;
    let request = protocol()
        .decode_prepare(payload, &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
        .unwrap();
    let GuestProcess::Command(argv) = request.process() else {
        panic!("expected command");
    };
    let mut bytes = [0; 64];
    let length = argv.write_nul_terminated_into(&mut bytes).unwrap();
    assert_eq!(
        &bytes[..length],
        "/bin/echo\0\u{10000}\u{10ffff}\0".as_bytes()
    );
}

fn protocol() -> NegotiatedProtocol {
    negotiate_hello(
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloOffer::local(ControlLimits::CURRENT),
    )
    .unwrap()
}

fn request() -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        2,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    )
}

fn resized(header: ControlFrameHeader, payload: &str) -> ControlFrameHeader {
    ControlFrameHeader::new_with_version(
        header.protocol_version(),
        header.kind(),
        header.request_id(),
        payload.len() as u32,
        header.descriptor_count(),
    )
    .unwrap()
}

#[test]
fn prepare_unit_variants_are_strings_and_command_remains_an_object() {
    let protocol = protocol();
    for execution in [
        r#""piped""#,
        r#""terminal""#,
        r#"{"piped":null}"#,
        r#"{ "terminal" : null }"#,
    ] {
        for process in [
            r#""login_shell""#,
            r#"{"login_shell":null}"#,
            r#"{"command":["/bin/true"]}"#,
        ] {
            let size = if execution.contains("terminal") {
                r#", "initial_size":{"cols":80,"rows":24}"#
            } else {
                ""
            };
            let payload = format!(
                r#"{{"user":"root","execution":{execution},"process":{process},"environment":[]{size}}}"#
            );
            let result =
                protocol.decode_prepare(payload.as_bytes(), &mut [0; MAX_PREPARE_UNESCAPE_BYTES]);
            assert_eq!(
                result.is_ok(),
                execution.starts_with('"') && process != r#"{"login_shell":null}"#,
                "{payload}"
            );
        }
    }
    let payload = br#"{"user":"root","exec\u0075tion":"piped","proc\u0065ss":"login_shell","environment":[{"name":"X","value":"{\"terminal\":null}"}]}"#;
    assert!(protocol
        .decode_prepare(payload, &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
        .is_ok());
    let invalid = std::str::from_utf8(payload)
        .unwrap()
        .replace(r#""login_shell""#, r#"{"login_shell":null}"#);
    assert!(protocol
        .decode_prepare(invalid.as_bytes(), &mut [0; MAX_PREPARE_UNESCAPE_BYTES])
        .is_err());
}

#[test]
fn cleanup_requested_is_a_string_not_a_unit_variant_object() {
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    for kind in [
        PreparationControlKind::Cancel,
        PreparationControlKind::Close,
    ] {
        let command = PreparationControlFrame::new(
            3,
            kind,
            PreparationControl::new(request.broker_id(), preparation),
        );
        let reply = ControlReplyFrame::new(command, ControlReplyStatus::CleanupRequested).unwrap();
        let mut bytes = [0; 4096];
        let (header, length) = protocol().encode_control_reply(&reply, &mut bytes).unwrap();
        let payload = std::str::from_utf8(&bytes[..length]).unwrap();
        assert!(protocol()
            .decode_control_reply(&command, header, payload.as_bytes(), 0)
            .is_ok());
        for alias in [
            r#"{"cleanup_requested":null}"#,
            r#"{ "cleanup_requested" : null }"#,
        ] {
            let invalid = payload.replace(r#""cleanup_requested""#, alias);
            assert!(protocol()
                .decode_control_reply(&command, resized(header, &invalid), invalid.as_bytes(), 0)
                .is_err());
        }
    }
}

#[test]
fn exit_unit_variants_are_strings_including_failure_stage() {
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let reports = [
        ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
        ExitReport::new(
            ExecutionStatus::NotStarted,
            None,
            CleanupStatus::InProgress,
            ExitReason::Cancelled,
        )
        .unwrap(),
        ExitReport::new(
            ExecutionStatus::Starting,
            None,
            CleanupStatus::InProgress,
            ExitReason::ClientClosed,
        )
        .unwrap(),
        ExitReport::new(
            ExecutionStatus::Unknown,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
        ExitReport::new(
            ExecutionStatus::Failed { errno: 2 },
            Some(ExitStatus::Exited(126)),
            CleanupStatus::Pending { errno: Some(5) },
            ExitReason::ExecFailed,
        )
        .unwrap()
        .with_failure(Some(GuestFailure::new(GuestFailureStage::Exec, 2).unwrap()))
        .unwrap(),
    ];
    for report in reports {
        let exit = ExitFrame::new(&request, preparation, report).unwrap();
        let mut bytes = [0; 4096];
        let (header, length) = protocol().encode_exit(&exit, &mut bytes).unwrap();
        let payload = std::str::from_utf8(&bytes[..length]).unwrap();
        assert!(protocol()
            .decode_exit(&request, preparation, header, payload.as_bytes(), 0)
            .is_ok());
        for field in [
            "started",
            "not_started",
            "starting",
            "unknown",
            "complete",
            "in_progress",
            "worker_exited",
            "cancelled",
            "client_closed",
            "exec_failed",
            "exec",
        ] {
            let token = format!("\"{field}\"");
            if !payload.contains(&token) {
                continue;
            }
            let invalid = payload.replace(&token, &format!("{{{token}:null}}"));
            assert!(
                protocol()
                    .decode_exit(
                        &request,
                        preparation,
                        resized(header, &invalid),
                        invalid.as_bytes(),
                        0
                    )
                    .is_err(),
                "{invalid}"
            );
        }
    }
}

#[test]
fn error_unit_variants_are_strings() {
    let request = request();
    let preparation = PreparationId::new([2; 16]);
    let error = ErrorFrame::new(
        3,
        request.broker_id(),
        Some(preparation),
        GuestOperation::Start,
        GuestErrorCode::System,
        Some(2),
    )
    .unwrap()
    .with_stage(Some(GuestFailureStage::Exec))
    .unwrap();
    let mut bytes = [0; 4096];
    let (header, length) = protocol().encode_error(&error, &mut bytes).unwrap();
    let request_header = ControlFrameHeader::new_with_version(
        header.protocol_version(),
        MessageKind::Start,
        3,
        0,
        0,
    )
    .unwrap();
    let payload = std::str::from_utf8(&bytes[..length]).unwrap();
    for field in ["start", "system", "exec"] {
        let token = format!("\"{field}\"");
        let invalid = payload.replace(&token, &format!("{{{token}:null}}"));
        assert!(protocol()
            .decode_error(
                request_header,
                &request,
                Some(preparation),
                resized(header, &invalid),
                invalid.as_bytes(),
                0
            )
            .is_err());
    }
}

#[test]
fn hello_rejection_unit_reason_is_a_string() {
    let rejected = HelloRejected {
        offer: HelloOffer::local(ControlLimits::CURRENT),
        reason: CompatibilityError::InvalidOffer,
    };
    let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = rejected.encode_payload(&mut bytes).unwrap();
    let payload = std::str::from_utf8(&bytes[..length]).unwrap();
    let invalid = payload.replace(r#""InvalidOffer""#, r#"{"InvalidOffer":null}"#);
    assert_ne!(payload, invalid);
    assert_eq!(
        HelloRejected::decode_payload(invalid.as_bytes()),
        Err(PayloadCodecError::InvalidJson)
    );
}
