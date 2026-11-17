extern crate std;

use super::*;
use crate::*;
use serde::{Deserialize, Serialize};
use std::boxed::Box;

const MARKER: FeatureSet = FeatureSet::from_bits(1 << 63);

pub(super) const fn fields(schema: SchemaId) -> super::codec::fields::SchemaFields {
    let required: &'static [&'static str] = match schema {
        SchemaId::TestPrepare => &["user", "execution", "process", "environment", "test_marker"],
        SchemaId::TestStart => &["broker_id", "preparation_id", "test_marker"],
        SchemaId::TestExit => &[
            "broker_id",
            "preparation_id",
            "execution",
            "cleanup",
            "reason",
            "test_marker",
        ],
        _ => panic!("expected a fixture schema"),
    };
    let optional: &'static [&'static str] = match schema {
        SchemaId::TestPrepare => &["initial_size"],
        SchemaId::TestStart => &["terminal_size", "execution_timeout_ms"],
        SchemaId::TestExit => &["exit", "failure"],
        _ => panic!("expected a fixture schema"),
    };
    let gated: &'static [FeatureField] = match schema {
        SchemaId::TestStart => &[
            FeatureField {
                path: "execution_timeout_ms",
                requires: FeatureSet::EXECUTION_TIMEOUT,
            },
            FeatureField {
                path: "test_marker",
                requires: MARKER,
            },
        ],
        _ => &[FeatureField {
            path: "test_marker",
            requires: MARKER,
        }],
    };
    super::codec::fields::SchemaFields {
        required,
        optional,
        gated,
    }
}

pub(super) fn has_marker(
    profile: &ProtocolProfile,
    kind: MessageKind,
    direction: Direction,
) -> bool {
    matches!(
        profile.schema(kind, direction).map(|schema| schema.codec),
        Some(SchemaId::TestPrepare | SchemaId::TestStart | SchemaId::TestExit)
    )
}

#[derive(Serialize)]
struct EncodePrepare<'a, 'b> {
    user: &'b str,
    execution: ExecutionKind,
    process: &'b GuestProcess<'a>,
    environment: &'b ProcessEnvironment<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_size: Option<TerminalSize>,
    test_marker: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum DecodeProcess<'a> {
    LoginShell,
    Command(#[serde(borrow)] heapless::Vec<&'a str, { MAX_COMMAND_ARGUMENTS + 1 }>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeEnvironment<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodePrepare<'a> {
    user: heapless::String<MAX_GUEST_USER_NAME_BYTES>,
    execution: ExecutionKind,
    #[serde(borrow)]
    process: DecodeProcess<'a>,
    #[serde(borrow)]
    environment: heapless::Vec<DecodeEnvironment<'a>, MAX_PROCESS_ENVIRONMENT_ENTRIES>,
    initial_size: Option<TerminalSize>,
    test_marker: bool,
}

pub(super) fn encode_prepare(
    profile: &ProtocolProfile,
    request: &PrepareRequest<'_>,
    output: &mut [u8],
) -> Result<usize, PayloadCodecError> {
    request.validate_with(profile.capacities())?;
    crate::json::encode_json_payload(
        &EncodePrepare {
            user: request.user(),
            execution: request.execution(),
            process: request.process(),
            environment: request.environment(),
            initial_size: request.initial_size(),
            test_marker: true,
        },
        output,
    )
}

pub(super) fn decode_prepare<'a>(
    profile: &ProtocolProfile,
    payload: &'a [u8],
    _: &mut [u8],
) -> Result<PrepareRequest<'a>, PayloadCodecError> {
    crate::json::validate_json_strings(payload)?;
    let (wire, _): (DecodePrepare<'a>, _) =
        serde_json_core::from_slice(payload).map_err(|_| PayloadCodecError::InvalidJson)?;
    if !wire.test_marker {
        return Err(PayloadCodecError::InvalidRequest);
    }
    if wire
        .environment
        .windows(2)
        .any(|pair| pair[0].name >= pair[1].name)
    {
        return Err(PayloadCodecError::InvalidRequest);
    }
    let process = match wire.process {
        DecodeProcess::LoginShell => GuestProcess::LoginShell,
        DecodeProcess::Command(argv) => GuestProcess::Command(CommandArgv::new(&argv)?),
    };
    let request = PrepareRequest::new(
        &wire.user,
        wire.execution,
        process,
        ProcessEnvironment::new(
            wire.environment
                .iter()
                .map(|entry| (entry.name, entry.value)),
        )?,
        wire.initial_size,
    )?;
    request.validate_with(profile.capacities())?;
    Ok(request)
}

pub(super) fn encode_prepare_frame(
    profile: &ProtocolProfile,
    frame: &PrepareRequestFrame<'_>,
    output: &mut [u8],
) -> Result<usize, PrepareFrameError> {
    let payload =
        output
            .get_mut(CONTROL_FRAME_HEADER_BYTES..)
            .ok_or(PrepareFrameError::Payload(
                PayloadCodecError::OutputTooSmall,
            ))?;
    let length =
        encode_prepare(profile, frame.request(), payload).map_err(PrepareFrameError::Payload)?;
    let header = ControlFrameHeader::new_with_version(
        profile.version(),
        MessageKind::PrepareRequest,
        frame.request_id(),
        length as u32,
        0,
    )
    .map_err(PrepareFrameError::Header)?;
    profile.validate_prepare_header(header)?;
    output[..CONTROL_FRAME_HEADER_BYTES].copy_from_slice(&header.encode());
    Ok(CONTROL_FRAME_HEADER_BYTES + length)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_prepare_into<'a, 's>(
    profile: &ProtocolProfile,
    storage: &'s mut PrepareDecodeStorage<'a>,
    header: ControlFrameHeader,
    payload: &'a [u8],
    descriptors: usize,
    broker: BrokerId,
    scratch: &mut [u8],
) -> Result<&'s PrepareRequestFrame<'a>, PrepareFrameError> {
    PrepareRequestFrame::validate_header(header, descriptors)?;
    header
        .validate_shape(payload.len(), descriptors)
        .map_err(PrepareFrameError::Frame)?;
    let request = decode_prepare(profile, payload, scratch).map_err(PrepareFrameError::Payload)?;
    Ok(storage.install_fixture(PrepareRequestFrame::new(
        header.request_id(),
        broker,
        request,
    )))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarkedStart {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_size: Option<TerminalSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_timeout_ms: Option<u64>,
    test_marker: bool,
}

pub(super) fn encode_start_payload(
    profile: &ProtocolProfile,
    start: &StartRequest,
    output: &mut [u8],
) -> Result<usize, PayloadCodecError> {
    super::codec::validate_timeout(start, profile.capacities())
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
    crate::json::encode_json_payload(
        &MarkedStart {
            broker_id: start.broker_id(),
            preparation_id: start.preparation_id(),
            terminal_size: start.terminal_size(),
            execution_timeout_ms: start.execution_timeout_ms(),
            test_marker: true,
        },
        output,
    )
}

pub(super) fn decode_start_payload(
    profile: &ProtocolProfile,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    payload: &[u8],
) -> Result<StartRequest, SessionPayloadError> {
    let wire: MarkedStart =
        crate::json::decode_json_payload(payload).map_err(SessionPayloadError::Payload)?;
    if !wire.test_marker {
        return Err(SessionPayloadError::InvalidResponse);
    }
    if wire.broker_id != request.broker_id() {
        return Err(SessionPayloadError::BrokerMismatch);
    }
    if wire.preparation_id != preparation {
        return Err(SessionPayloadError::PreparationMismatch);
    }
    let start = StartRequest::new(
        request,
        preparation,
        wire.terminal_size,
        wire.execution_timeout_ms,
    )?;
    super::codec::validate_timeout(&start, profile.capacities())?;
    Ok(start)
}

pub(super) fn encode_start(
    profile: &ProtocolProfile,
    frame: &StartFrame,
    output: &mut [u8],
) -> Option<Result<(ControlFrameHeader, usize), SessionFrameError>> {
    if !has_marker(profile, MessageKind::Start, Direction::HostToGuest) {
        return None;
    }
    Some((|| {
        let length = encode_start_payload(profile, frame.request(), output)
            .map_err(|error| SessionFrameError::Payload(SessionPayloadError::Payload(error)))?;
        envelope(profile, MessageKind::Start, frame.request_id(), length)
    })())
}

pub(super) fn decode_start(
    profile: &ProtocolProfile,
    request_id: u64,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: usize,
) -> Option<Result<StartFrame, SessionFrameError>> {
    if !has_marker(profile, MessageKind::Start, Direction::HostToGuest) {
        return None;
    }
    Some((|| {
        check_envelope(header, payload, descriptors, request_id)?;
        Ok(StartFrame::new(
            request_id,
            decode_start_payload(profile, request, preparation, payload)
                .map_err(SessionFrameError::Payload)?,
        ))
    })())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarkedExit {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    execution: ExecutionStatus,
    exit: Option<ExitStatus>,
    cleanup: CleanupStatus,
    reason: ExitReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<GuestFailure>,
    test_marker: bool,
}

pub(super) fn encode_exit(
    profile: &ProtocolProfile,
    frame: &ExitFrame,
    output: &mut [u8],
) -> Option<Result<(ControlFrameHeader, usize), SessionFrameError>> {
    if !has_marker(profile, MessageKind::Exit, Direction::GuestToHost) {
        return None;
    }
    Some((|| {
        let (broker_id, preparation_id) = frame.fixture_token();
        let report = frame.report();
        let wire = MarkedExit {
            broker_id,
            preparation_id,
            execution: report.execution(),
            exit: report.exit(),
            cleanup: report.cleanup(),
            reason: report.reason(),
            failure: report.failure(),
            test_marker: true,
        };
        let length = crate::json::encode_json_payload(&wire, output)
            .map_err(|error| SessionFrameError::Payload(SessionPayloadError::Payload(error)))?;
        envelope(profile, MessageKind::Exit, frame.request_id(), length)
    })())
}

pub(super) fn decode_exit(
    profile: &ProtocolProfile,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: usize,
) -> Option<Result<ExitFrame, SessionFrameError>> {
    if !has_marker(profile, MessageKind::Exit, Direction::GuestToHost) {
        return None;
    }
    Some((|| {
        check_envelope(header, payload, descriptors, request.request_id())?;
        let wire: MarkedExit = crate::json::decode_json_payload(payload)
            .map_err(|error| SessionFrameError::Payload(SessionPayloadError::Payload(error)))?;
        if !wire.test_marker {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::InvalidResponse,
            ));
        }
        if wire.broker_id != request.broker_id() {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::BrokerMismatch,
            ));
        }
        if wire.preparation_id != preparation {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::PreparationMismatch,
            ));
        }
        let report = ExitReport::new(wire.execution, wire.exit, wire.cleanup, wire.reason)
            .and_then(|report| report.with_failure(wire.failure))
            .map_err(SessionFrameError::Payload)?;
        ExitFrame::new(request, preparation, report).map_err(SessionFrameError::Payload)
    })())
}

fn envelope(
    profile: &ProtocolProfile,
    kind: MessageKind,
    id: u64,
    length: usize,
) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
    let header =
        ControlFrameHeader::new_with_version(profile.version(), kind, id, length as u32, 0)
            .map_err(SessionFrameError::Header)?;
    profile.validate_session_header(header)?;
    Ok((header, length))
}

fn check_envelope(
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: usize,
    id: u64,
) -> Result<(), SessionFrameError> {
    if header.request_id() != id {
        return Err(SessionFrameError::MismatchedRequestId {
            expected: id,
            actual: header.request_id(),
        });
    }
    if descriptors != 0 || header.descriptor_count() != 0 {
        return Err(SessionFrameError::UnexpectedDescriptors {
            declared: header.descriptor_count(),
            received: descriptors,
        });
    }
    header
        .validate_shape(payload.len(), descriptors)
        .map_err(SessionFrameError::Frame)
}

fn profiles() -> &'static [ProtocolProfile] {
    let base = protocol_profiles()[0];
    let mut messages = base.messages().to_vec();
    for schema in &mut messages {
        schema.codec = match schema.codec {
            SchemaId::PrepareV1 => SchemaId::TestPrepare,
            SchemaId::StartV1 => SchemaId::TestStart,
            SchemaId::ExitV1 => SchemaId::TestExit,
            other => other,
        };
        if matches!(
            schema.codec,
            SchemaId::TestPrepare | SchemaId::TestStart | SchemaId::TestExit
        ) {
            let fields = schema.codec.fields();
            schema.required_fields = fields.required;
            schema.optional_fields = fields.optional;
            schema.gated_fields = fields.gated;
        }
    }
    Box::leak(Box::new([
        ProtocolProfile {
            capacities: ProfileCapacities {
                environment_bytes: 64,
                max_control_frame_bytes: 4096,
                ..*base.capacities()
            },
            ..base
        },
        ProtocolProfile {
            version: ProtocolVersion { major: 1, minor: 1 },
            features: base.features().union(MARKER),
            messages: Box::leak(messages.into_boxed_slice()),
            capacities: ProfileCapacities {
                max_control_frame_bytes: 8192,
                ..*base.capacities()
            },
            ..base
        },
    ]))
}

fn select(profiles: &'static [ProtocolProfile], minor: u16, marker: bool) -> NegotiatedProtocol {
    let guest = offer_from_profiles(profiles, ControlLimits::CURRENT);
    let host = HelloOffer {
        protocols: ProtocolRange {
            max_minor: minor,
            ..guest.protocols
        },
        supported_features: if marker {
            guest.supported_features
        } else {
            guest
                .supported_features
                .intersection(FeatureSet::from_bits(!MARKER.bits()))
        },
        ..guest
    };
    let request = HelloRequest::from_offer(host, FeatureSet::EMPTY).unwrap();
    let (response, selected) =
        HelloResponse::accept_with_profiles(request, guest, BrokerId::new([1; 16]), profiles)
            .unwrap();
    assert_eq!(
        request
            .negotiate_protocol_with_profiles(ProtocolVersion::BOOTSTRAP, response, profiles)
            .unwrap(),
        selected
    );
    selected
}

fn request(environment: ProcessEnvironment<'_>) -> PrepareRequestFrame<'_> {
    PrepareRequestFrame::new(
        11,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
            environment,
            None,
        )
        .unwrap(),
    )
}

#[test]
fn negotiated_schema_changes_prepare_start_and_exit_payloads() {
    let profiles = profiles();
    let low = select(profiles, 0, true);
    let high = select(profiles, 1, true);
    let prepare = request(ProcessEnvironment::default());
    let preparation = PreparationId::new([2; 16]);
    let start = StartFrame::new(
        12,
        StartRequest::new(&prepare, preparation, None, None).unwrap(),
    );
    let exit = ExitFrame::new(
        &prepare,
        preparation,
        ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(0)),
            CleanupStatus::Complete,
            ExitReason::WorkerExited,
        )
        .unwrap(),
    )
    .unwrap();
    let mut output = [0; 8192];
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    for (selected, other, marked) in [(high, low, true), (low, high, false)] {
        let length = selected
            .encode_prepare_frame(&prepare, &mut output)
            .unwrap();
        let header = ControlFrameHeader::decode(&output[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
        let payload = &output[CONTROL_FRAME_HEADER_BYTES..length];
        assert_eq!(
            std::str::from_utf8(payload)
                .unwrap()
                .contains("test_marker"),
            marked
        );
        selected
            .decode_prepare_frame(header, payload, 0, prepare.broker_id(), &mut scratch)
            .unwrap();
        let remapped = ControlFrameHeader::new_with_version(
            other.profile().version(),
            header.kind(),
            header.request_id(),
            header.payload_bytes(),
            0,
        )
        .unwrap();
        assert!(other
            .decode_prepare_frame(remapped, payload, 0, prepare.broker_id(), &mut scratch)
            .is_err());
        let (header, length) = selected.encode_start_frame(&start, &mut output).unwrap();
        assert_eq!(
            std::str::from_utf8(&output[..length])
                .unwrap()
                .contains("test_marker"),
            marked
        );
        selected
            .decode_start_frame(12, &prepare, preparation, header, &output[..length], 0)
            .unwrap();
        let remapped = ControlFrameHeader::new_with_version(
            other.profile().version(),
            header.kind(),
            header.request_id(),
            header.payload_bytes(),
            0,
        )
        .unwrap();
        assert!(other
            .decode_start_frame(12, &prepare, preparation, remapped, &output[..length], 0)
            .is_err());
        let (header, length) = selected.encode_exit(&exit, &mut output).unwrap();
        assert_eq!(
            std::str::from_utf8(&output[..length])
                .unwrap()
                .contains("test_marker"),
            marked
        );
        selected
            .decode_exit(&prepare, preparation, header, &output[..length], 0)
            .unwrap();
        let remapped = ControlFrameHeader::new_with_version(
            other.profile().version(),
            header.kind(),
            header.request_id(),
            header.payload_bytes(),
            0,
        )
        .unwrap();
        assert!(other
            .decode_exit(&prepare, preparation, remapped, &output[..length], 0)
            .is_err());
    }
    let disabled = select(profiles, 1, false);
    assert!(disabled
        .encode_prepare(prepare.request(), &mut output)
        .is_err());
    assert!(disabled.encode_start_frame(&start, &mut output).is_err());
    assert!(disabled.encode_exit(&exit, &mut output).is_err());
    let length = high.encode_prepare(prepare.request(), &mut output).unwrap();
    assert!(disabled
        .decode_prepare(&output[..length], &mut scratch)
        .is_err());
    let length = high
        .encode_start_payload(start.request(), &mut output)
        .unwrap();
    assert_eq!(
        high.decode_start(&prepare, preparation, &output[..length])
            .unwrap(),
        *start.request()
    );
    assert!(disabled
        .decode_start(&prepare, preparation, &output[..length])
        .is_err());
    let (header, length) = high.encode_exit(&exit, &mut output).unwrap();
    assert!(disabled
        .decode_exit(&prepare, preparation, header, &output[..length], 0)
        .is_err());
}

#[test]
fn selected_capacities_apply_before_submission_and_during_decoding() {
    let profiles = profiles();
    let low = select(profiles, 0, true);
    let high = select(profiles, 1, true);
    let value = "x".repeat(100);
    let prepare = request(ProcessEnvironment::new([("VALUE", value.as_str())]).unwrap());
    let mut bytes = [0; 8192];
    assert_eq!(
        low.encode_prepare(prepare.request(), &mut bytes),
        Err(PayloadCodecError::InvalidRequest)
    );
    let length = high.encode_prepare(prepare.request(), &mut bytes).unwrap();
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    high.decode_prepare(&bytes[..length], &mut scratch).unwrap();
    let length = prepare.request().encode_payload(&mut bytes).unwrap();
    assert_eq!(
        low.decode_prepare(&bytes[..length], &mut scratch)
            .unwrap_err(),
        PayloadCodecError::InvalidRequest
    );

    let value = "x".repeat(2000);
    let limited = NegotiatedProtocol {
        limits: EnforcedControlLimits::new(ControlLimits::new(4096, 4).unwrap()),
        ..high
    };
    let argv = ["/bin/true", value.as_str(), value.as_str()];
    let prepare = PrepareRequestFrame::new(
        11,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(&argv).unwrap()),
            ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    );
    let length = high.encode_prepare_frame(&prepare, &mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    assert!(matches!(
        limited.encode_prepare_frame(&prepare, &mut [0; 8192]),
        Err(PrepareFrameError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));
    assert!(matches!(
        limited.decode_prepare_frame(
            header,
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            0,
            prepare.broker_id(),
            &mut scratch
        ),
        Err(PrepareFrameError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));
    assert!(matches!(
        low.encode_prepare_frame(&prepare, &mut [0; 8192]),
        Err(PrepareFrameError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));
    let length = low
        .profile()
        .encode_prepare(prepare.request(), &mut bytes)
        .unwrap();
    let header = ControlFrameHeader::new_with_version(
        low.profile().version(),
        MessageKind::PrepareRequest,
        prepare.request_id(),
        length as u32,
        0,
    )
    .unwrap();
    assert!(matches!(
        low.decode_prepare_frame(
            header,
            &bytes[..length],
            0,
            prepare.broker_id(),
            &mut scratch
        ),
        Err(PrepareFrameError::Limit(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));
}
