extern crate std;

use super::*;
use crate::*;
use std::boxed::Box;

fn selected(entries: usize, bytes: usize) -> NegotiatedProtocol {
    let base = protocol_profiles()[0];
    let profiles = Box::leak(Box::new([ProtocolProfile {
        capacities: ProfileCapacities {
            environment_entries: entries,
            environment_bytes: bytes,
            ..*base.capacities()
        },
        ..base
    }]));
    let offer = offer_from_profiles(profiles, ControlLimits::CURRENT);
    negotiate_with(
        HelloRequest::from_offer(offer, FeatureSet::EMPTY).unwrap(),
        offer,
        profiles,
    )
    .unwrap()
}

#[test]
fn environment_stops_at_the_first_profile_or_ordering_violation() {
    for (protocol, entries) in [
        (
            selected(1, 64),
            r#"{"name":"A","value":""},{"name":"B","value":""}"#,
        ),
        (selected(32, 4), r#"{"name":"A","value":"xx"}"#),
        (
            selected(32, 64),
            r#"{"name":"A","value":""},{"name":"\u0041","value":""}"#,
        ),
        (
            selected(32, 64),
            r#"{"name":"B","value":""},{"name":"A","value":""}"#,
        ),
    ] {
        // The malformed next item must not mask the preceding field error.
        let payload = std::format!(
            r#"{{"user":"root","execution":"piped","process":"login_shell","environment":[{entries},false]}}"#
        );
        let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
        assert_eq!(
            protocol
                .decode_prepare(payload.as_bytes(), &mut scratch)
                .unwrap_err(),
            PayloadCodecError::InvalidRequest
        );
        let header = ControlFrameHeader::new_with_version(
            protocol.profile().version(),
            MessageKind::PrepareRequest,
            2,
            payload.len() as u32,
            0,
        )
        .unwrap();
        let mut storage = PrepareDecodeStorage::new();
        assert_eq!(
            protocol
                .decode_prepare_into(
                    &mut storage,
                    header,
                    payload.as_bytes(),
                    0,
                    BrokerId::new([1; 16]),
                    &mut scratch
                )
                .unwrap_err(),
            PrepareFrameError::Payload(PayloadCodecError::InvalidRequest)
        );
        assert!(storage.request().is_none());
    }
}

#[test]
fn environment_accepts_exact_selected_limits() {
    let protocol = selected(1, 4);
    let payload = br#"{"user":"root","execution":"piped","process":"login_shell","environment":[{"name":"A","value":"x"}]}"#;
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    let request = protocol.decode_prepare(payload, &mut scratch).unwrap();
    assert_eq!(request.environment().entries().len(), 1);
    let mut bytes = [0; 4096];
    protocol.encode_prepare(&request, &mut bytes).unwrap();
}
