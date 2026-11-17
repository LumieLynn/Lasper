extern crate std;
use super::*;
use crate::{FeatureSet, HELLO_PAYLOAD_BUFFER_BYTES};

#[test]
fn bootstrap_numbers_follow_json_number_syntax() {
    for invalid in ["01", "+1", "1.", "0x1", "1e0"] {
        let payload = std::format!(
            r#"{{"offer":{{"protocols":{{"major":{invalid},"min_minor":0,"max_minor":0}},"supported_features":3,"limits":{{"max_control_frame_bytes":4096,"max_descriptors":4}}}},"required_features":0}}"#
        );
        assert!(
            HelloRequest::decode_payload(payload.as_bytes()).is_err(),
            "accepted {invalid}"
        );
    }
}

fn maximum_offer() -> crate::HelloOffer {
    crate::HelloOffer {
        protocols: crate::ProtocolRange {
            major: u16::MAX,
            min_minor: u16::MAX,
            max_minor: u16::MAX,
        },
        supported_features: FeatureSet::from_bits(u64::MAX),
        limits: ControlLimits::new(u32::MAX, u16::MAX).unwrap(),
    }
}

#[test]
fn hello_request_round_trips_as_bounded_json() {
    let request = HelloRequest::new(FeatureSet::from_bits(0b11), ControlLimits::CURRENT).unwrap();
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

    let length = request.encode_payload(&mut payload).unwrap();
    assert_eq!(
        &payload[..length],
        br#"{"offer":{"protocols":{"major":1,"min_minor":0,"max_minor":0},"supported_features":3,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":4}},"required_features":3}"#
    );
    let decoded = HelloRequest::decode_payload(&payload[..length]).unwrap();

    assert_eq!(decoded, request);
}

#[test]
fn hello_response_round_trips_broker_generation_and_limits() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let (response, _) = HelloResponse::accept(
        request,
        crate::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new(*b"broker-generatio"),
    )
    .unwrap();
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

    let length = response.encode_payload(&mut payload).unwrap();
    let decoded = HelloResponse::decode_payload(&payload[..length]).unwrap();

    assert_eq!(decoded, response);
}

#[test]
fn hello_buffer_fits_maximum_response_fields() {
    let response = HelloResponse {
        offer: maximum_offer(),
        selected: crate::HelloSelection {
            version: ProtocolVersion {
                major: u16::MAX,
                minor: u16::MAX,
            },
            features: FeatureSet::from_bits(u64::MAX),
            limits: maximum_offer().limits,
        },
        broker_id: BrokerId::new([u8::MAX; BROKER_ID_BYTES]),
    };
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

    let length = response.encode_payload(&mut payload).unwrap();
    assert_eq!(
        HelloResponse::decode_payload(&payload[..length]),
        Ok(response)
    );
}

#[test]
fn hello_buffer_fits_maximum_request_fields() {
    let request =
        HelloRequest::from_offer(maximum_offer(), FeatureSet::from_bits(u64::MAX)).unwrap();
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

    let length = request.encode_payload(&mut payload).unwrap();
    assert_eq!(
        HelloRequest::decode_payload(&payload[..length]),
        Ok(request)
    );
}

#[test]
fn hello_buffer_fits_every_rejection_with_maximum_fields() {
    use crate::CompatibilityError;
    let offer = maximum_offer();
    for reason in [
        CompatibilityError::MajorMismatch {
            local: u16::MAX,
            peer: u16::MAX,
        },
        CompatibilityError::NoCommonMinor {
            local: offer.protocols,
            peer: offer.protocols,
        },
        CompatibilityError::MissingFeatures {
            required: FeatureSet::from_bits(u64::MAX),
            available: FeatureSet::from_bits(u64::MAX),
        },
        CompatibilityError::InsufficientFrameCapacity {
            required: u32::MAX,
            available: u32::MAX,
        },
        CompatibilityError::InvalidOffer,
    ] {
        let rejected = HelloRejected { offer, reason };
        let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];
        let length = rejected.encode_payload(&mut payload).unwrap();
        assert_eq!(
            HelloRejected::decode_payload(&payload[..length]),
            Ok(rejected)
        );
    }
}

#[test]
fn hello_payload_decoders_enforce_the_bootstrap_budget() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let (response, _) = HelloResponse::accept(
        request,
        crate::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([1; 16]),
    )
    .unwrap();
    let rejected = HelloRejected {
        offer: response.offer(),
        reason: crate::CompatibilityError::InvalidOffer,
    };
    let mut payload = [b' '; HELLO_PAYLOAD_BUFFER_BYTES + 1];
    request.encode_payload(&mut payload).unwrap();
    assert!(HelloRequest::decode_payload(&payload[..HELLO_PAYLOAD_BUFFER_BYTES]).is_ok());
    assert_eq!(
        HelloRequest::decode_payload(&payload),
        Err(PayloadCodecError::PayloadTooLarge)
    );
    payload.fill(b' ');
    response.encode_payload(&mut payload).unwrap();
    assert!(HelloResponse::decode_payload(&payload[..HELLO_PAYLOAD_BUFFER_BYTES]).is_ok());
    assert_eq!(
        HelloResponse::decode_payload(&payload),
        Err(PayloadCodecError::PayloadTooLarge)
    );
    payload.fill(b' ');
    rejected.encode_payload(&mut payload).unwrap();
    assert!(HelloRejected::decode_payload(&payload[..HELLO_PAYLOAD_BUFFER_BYTES]).is_ok());
    assert_eq!(
        HelloRejected::decode_payload(&payload),
        Err(PayloadCodecError::PayloadTooLarge)
    );
}

#[test]
fn decoder_accepts_json_whitespace_but_rejects_trailing_values() {
    let payload = br#"{"offer":{"protocols":{"major":1,"min_minor":0,"max_minor":0},"supported_features":3,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}},"required_features":0}  "#;
    assert!(HelloRequest::decode_payload(payload).is_ok());

    let mut trailing = payload.to_vec();
    trailing.extend_from_slice(br#"{"extra":true}"#);
    assert_eq!(
        HelloRequest::decode_payload(&trailing),
        Err(PayloadCodecError::InvalidJson)
    );
}

#[test]
fn hello_rejects_duplicate_unknown_and_malformed_fields() {
    let duplicate = br#"{"required_features":0,"required_features":1,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}}"#;
    let unknown = br#"{"offer":{"protocols":{"major":1,"min_minor":0,"max_minor":0},"supported_features":3,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}},"required_features":0,"extra":true}"#;
    let malformed = br#"{"offer":{"protocols":{"major":1,"min_minor":0,"max_minor":0},"supported_features":3,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}},"required_features":-1}"#;

    assert_eq!(
        HelloRequest::decode_payload(duplicate),
        Err(PayloadCodecError::InvalidJson)
    );
    assert_eq!(
        HelloRequest::decode_payload(unknown),
        Err(PayloadCodecError::InvalidJson)
    );
    assert_eq!(
        HelloRequest::decode_payload(malformed),
        Err(PayloadCodecError::InvalidJson)
    );
}

#[test]
fn invalid_request_offer_is_rejected_by_negotiation_before_selection() {
    let too_small = br#"{"offer":{"protocols":{"major":1,"min_minor":0,"max_minor":0},"supported_features":3,"limits":{"max_control_frame_bytes":31,"max_descriptors":0}},"required_features":0}"#;

    let request = HelloRequest::decode_payload(too_small).unwrap();
    assert_eq!(
        crate::negotiate_hello(request, crate::HelloOffer::local(ControlLimits::CURRENT)),
        Err(crate::CompatibilityError::InvalidOffer)
    );
    assert_eq!(
        request.encode_payload(&mut [0; HELLO_PAYLOAD_BUFFER_BYTES]),
        Err(PayloadCodecError::InvalidLimits)
    );
}

#[test]
fn semantically_invalid_host_offers_receive_a_valid_invalid_offer_rejection() {
    for (low, high, supported, required, capacity) in
        [(9, 0, 3, 0, 4096), (0, 0, 1, 2, 4096), (0, 0, 3, 0, 31)]
    {
        let payload = std::format!(
            r#"{{"offer":{{"protocols":{{"major":1,"min_minor":{low},"max_minor":{high}}},"supported_features":{supported},"limits":{{"max_control_frame_bytes":{capacity},"max_descriptors":4}}}},"required_features":{required}}}"#
        );
        let request = HelloRequest::decode_payload(payload.as_bytes()).unwrap();
        let guest = crate::HelloOffer::local(ControlLimits::CURRENT);
        let reason = crate::negotiate_hello(request, guest).unwrap_err();
        assert_eq!(reason, crate::CompatibilityError::InvalidOffer);
        let rejected = HelloRejected {
            offer: guest,
            reason,
        };
        let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
        let length = rejected.encode_payload(&mut bytes).unwrap();
        let rejected = HelloRejected::decode_payload(&bytes[..length]).unwrap();
        assert_eq!(
            rejected.validate_for(request, ProtocolVersion::BOOTSTRAP),
            Ok(())
        );
    }
}

#[test]
fn rejection_requires_a_valid_guest_offer_on_encode_and_decode() {
    for (range, frame, expected) in [
        ((9, 0), 4096, PayloadCodecError::InvalidRequest),
        ((0, 0), 31, PayloadCodecError::InvalidLimits),
    ] {
        let payload = std::format!(
            "{{\"offer\":{{\"protocols\":{{\"major\":1,\"min_minor\":{},\"max_minor\":{}}},\"supported_features\":3,\"limits\":{{\"max_control_frame_bytes\":{},\"max_descriptors\":4}}}},\"reason\":\"InvalidOffer\"}}",
            range.0, range.1, frame,
        );
        assert_eq!(
            HelloRejected::decode_payload(payload.as_bytes()),
            Err(expected)
        );
        let rejected: HelloRejected = crate::json::decode_json_payload(payload.as_bytes()).unwrap();
        assert_eq!(
            rejected.encode_payload(&mut [0; HELLO_PAYLOAD_BUFFER_BYTES]),
            Err(expected)
        );
        let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
        assert_eq!(
            rejected.validate_for(request, ProtocolVersion::BOOTSTRAP),
            Err(crate::CompatibilityError::InvalidSelection)
        );
    }
}

#[test]
fn rejection_binds_the_reason_and_header_to_the_original_request() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let mut offer = request.offer();
    offer.protocols.major = 7;
    let rejected = HelloRejected {
        offer,
        reason: crate::CompatibilityError::MajorMismatch { local: 7, peer: 1 },
    };
    let version = ProtocolVersion { major: 7, minor: 0 };
    assert_eq!(rejected.validate_for(request, version), Ok(()));
    assert_eq!(
        rejected.validate_for(request, ProtocolVersion::BOOTSTRAP),
        Err(crate::CompatibilityError::InvalidSelection)
    );
    assert_eq!(
        HelloRejected {
            reason: crate::CompatibilityError::InvalidOffer,
            ..rejected
        }
        .validate_for(request, version),
        Err(crate::CompatibilityError::InvalidSelection)
    );
}

#[test]
fn negotiation_intersects_limits_and_rejects_missing_features() {
    let request = HelloRequest::new(
        FeatureSet::EXECUTION_TIMEOUT,
        ControlLimits::new(65536, 4).unwrap(),
    )
    .unwrap();
    let offer = crate::HelloOffer::local(ControlLimits::new(32768, 3).unwrap());
    let (response, selected) =
        HelloResponse::accept(request, offer, BrokerId::new([1; 16])).unwrap();
    assert_eq!(
        request
            .negotiate_protocol(ProtocolVersion { major: 1, minor: 0 }, response)
            .unwrap()
            .limits()
            .get(),
        selected.limits().get()
    );
    assert_eq!(
        selected.limits().get(),
        ControlLimits::new(32768, 3).unwrap()
    );
    let missing = crate::HelloOffer {
        supported_features: FeatureSet::WORKER_PIDFD,
        ..offer
    };
    assert!(matches!(
        crate::negotiate_hello(request, missing),
        Err(crate::CompatibilityError::MissingFeatures { .. })
    ));
    let mut changed = response;
    changed.selected.features = FeatureSet::WORKER_PIDFD;
    assert_eq!(
        request.negotiate_protocol(ProtocolVersion { major: 1, minor: 0 }, changed),
        Err(crate::CompatibilityError::InvalidSelection)
    );
}
