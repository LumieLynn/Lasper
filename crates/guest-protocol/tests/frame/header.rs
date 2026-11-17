use super::*;

#[test]
fn control_frame_round_trips_a_borrowed_payload() {
    let payload = b"{\"operation\":\"prepare\"}";
    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        17,
        payload.len() as u32,
        0,
    )
    .unwrap();
    let mut output = [0; CONTROL_FRAME_HEADER_BYTES + 64];

    let frame_bytes = header.encode_frame(payload, &mut output).unwrap();
    let decoded = ControlFrame::decode(&output[..frame_bytes]).unwrap();

    assert_eq!(decoded.header(), header);
    assert_eq!(decoded.payload(), payload);
}

#[test]
fn control_frame_encoding_requires_matching_lengths_and_capacity() {
    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        17,
        4,
        0,
    )
    .unwrap();
    let mut output = [0; CONTROL_FRAME_HEADER_BYTES + 2];

    assert_eq!(
        header.encode_frame(b"abc", &mut output),
        Err(ControlFrameError::PayloadLengthMismatch {
            declared: 4,
            actual: 3,
        })
    );
    let descriptor_header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        17,
        3,
        0,
    )
    .unwrap();
    assert_eq!(
        descriptor_header.validate_shape(3, 1),
        Err(ControlFrameError::DescriptorCountMismatch {
            declared: 0,
            actual: 1,
        })
    );

    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        17,
        3,
        0,
    )
    .unwrap();
    assert_eq!(
        header.encode_frame(b"abc", &mut output),
        Err(ControlFrameError::OutputTooSmall {
            required: CONTROL_FRAME_HEADER_BYTES + 3,
            available: CONTROL_FRAME_HEADER_BYTES + 2,
        })
    );
}

#[test]
fn control_frame_decoding_rejects_incomplete_and_trailing_data() {
    assert_eq!(
        ControlFrame::decode(&[0; CONTROL_FRAME_HEADER_BYTES - 1]),
        Err(ControlFrameError::TruncatedHeader {
            actual: CONTROL_FRAME_HEADER_BYTES - 1,
        })
    );

    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        17,
        3,
        0,
    )
    .unwrap();
    let mut encoded = [0; CONTROL_FRAME_HEADER_BYTES + 3];
    header.encode_frame(b"abc", &mut encoded).unwrap();

    assert_eq!(
        ControlFrame::decode(&encoded[..CONTROL_FRAME_HEADER_BYTES + 2]),
        Err(ControlFrameError::TruncatedPayload {
            declared: 3,
            available: 2,
        })
    );
    let mut trailing = [0; CONTROL_FRAME_HEADER_BYTES + 4];
    trailing[..encoded.len()].copy_from_slice(&encoded);
    assert_eq!(
        ControlFrame::decode(&trailing),
        Err(ControlFrameError::TrailingBytes {
            expected: CONTROL_FRAME_HEADER_BYTES + 3,
            actual: CONTROL_FRAME_HEADER_BYTES + 4,
        })
    );
}

#[test]
fn control_frame_prefix_decoding_preserves_a_coalesced_next_frame() {
    let first_payload = b"first";
    let second_payload = b"second";
    let first = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        first_payload.len() as u32,
        0,
    )
    .unwrap();
    let second = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloResponse,
        2,
        second_payload.len() as u32,
        0,
    )
    .unwrap();
    let first_bytes = CONTROL_FRAME_HEADER_BYTES + first_payload.len();
    let total_bytes = first_bytes + CONTROL_FRAME_HEADER_BYTES + second_payload.len();
    let mut stream = [0; CONTROL_FRAME_HEADER_BYTES * 2 + 11];
    first
        .encode_frame(first_payload, &mut stream[..first_bytes])
        .unwrap();
    second
        .encode_frame(second_payload, &mut stream[first_bytes..total_bytes])
        .unwrap();

    let (decoded_first, consumed) = ControlFrame::decode_prefix(&stream[..total_bytes]).unwrap();
    let decoded_second = ControlFrame::decode(&stream[consumed..total_bytes]).unwrap();

    assert_eq!(decoded_first.header(), first);
    assert_eq!(decoded_first.payload(), first_payload);
    assert_eq!(decoded_second.header(), second);
    assert_eq!(decoded_second.payload(), second_payload);
}

#[test]
fn control_header_round_trips() {
    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        42,
        4096,
        2,
    )
    .unwrap();

    assert_eq!(ControlFrameHeader::decode(&header.encode()), Ok(header));
}

#[test]
fn control_header_keeps_peer_protocol_version_for_negotiation() {
    let mut encoded = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        7,
        0,
        0,
    )
    .unwrap()
    .encode();
    encoded[12..14].copy_from_slice(&9u16.to_le_bytes());
    encoded[14..16].copy_from_slice(&3u16.to_le_bytes());

    let decoded = ControlFrameHeader::decode(&encoded).unwrap();
    assert_eq!(decoded.protocol_major(), 9);
    assert_eq!(decoded.protocol_minor(), 3);
}

#[test]
fn control_header_rejects_unbounded_payloads_and_descriptors() {
    assert_eq!(
        ControlFrameHeader::new_with_version(
            crate::ProtocolVersion { major: 1, minor: 0 },
            MessageKind::PrepareRequest,
            1,
            MAX_CONTROL_PAYLOAD_BYTES + 1,
            0,
        ),
        Err(ControlFrameHeaderError::PayloadTooLarge)
    );
    assert_eq!(
        ControlFrameHeader::new_with_version(
            crate::ProtocolVersion { major: 1, minor: 0 },
            MessageKind::PrepareReady,
            1,
            0,
            MAX_CONTROL_DESCRIPTORS + 1,
        ),
        Err(ControlFrameHeaderError::TooManyDescriptors)
    );

    let mut payload = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        1,
        0,
        0,
    )
    .unwrap()
    .encode();
    payload[24..28].copy_from_slice(&(MAX_CONTROL_PAYLOAD_BYTES + 1).to_le_bytes());
    assert_eq!(
        ControlFrameHeader::decode(&payload),
        Err(ControlFrameHeaderError::PayloadTooLarge)
    );

    let mut descriptors = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        1,
        0,
        0,
    )
    .unwrap()
    .encode();
    descriptors[28..30].copy_from_slice(&(MAX_CONTROL_DESCRIPTORS + 1).to_le_bytes());
    assert_eq!(
        ControlFrameHeader::decode(&descriptors),
        Err(ControlFrameHeaderError::TooManyDescriptors)
    );
}

#[test]
fn control_header_rejects_unknown_kinds_and_reserved_bits() {
    let mut kind = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        0,
        0,
    )
    .unwrap()
    .encode();
    kind[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(
        ControlFrameHeader::decode(&kind),
        Err(ControlFrameHeaderError::UnknownMessageKind)
    );

    let mut reserved = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        0,
        0,
    )
    .unwrap()
    .encode();
    reserved[31] = 1;
    assert_eq!(
        ControlFrameHeader::decode(&reserved),
        Err(ControlFrameHeaderError::ReservedBytesSet)
    );
}
