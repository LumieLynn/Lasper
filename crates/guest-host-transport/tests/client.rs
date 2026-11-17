use lasper_guest_host_transport::{
    ClientOptions, GuestClient, PrepareCapacityError, PrepareError, ProcessSpec, Program,
    StartSubmission, Stdio,
};
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, ExecutionKind, FeatureSet, GuestProcess,
    HelloOffer, HelloRequest, HelloResponse, MessageKind, PrepareRequest, ProcessEnvironment,
    CONTROL_FRAME_HEADER_BYTES, HELLO_PAYLOAD_BUFFER_BYTES,
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    time::Duration,
};
use tokio::time::Instant;

#[path = "support/target.rs"]
mod target;

#[tokio::test]
async fn client_rejects_invalid_paths_and_unrepresentable_budgets() {
    let target = target::current_target().await;
    for endpoint in ["relative", "/a/../b", "/./broker", "/broker\0", "/broker\n"] {
        let error = GuestClient::new(
            target.clone(),
            ClientOptions {
                endpoint: endpoint.into(),
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.field, "endpoint");
    }
    for field in 0..4 {
        for duration in [Duration::ZERO, Duration::MAX] {
            let mut options = ClientOptions::default();
            match field {
                0 => options.timeouts.connect_hello = duration,
                1 => options.timeouts.prepare = duration,
                2 => options.timeouts.operation = duration,
                _ => options.timeouts.cleanup_observation = duration,
            }
            assert_eq!(
                GuestClient::new(target.clone(), options)
                    .err()
                    .unwrap()
                    .field,
                "timeouts"
            );
        }
    }
}

#[test]
fn registered_prepare_fields_fit_the_profile_frame_even_with_maximum_json_expansion() {
    for profile in lasper_guest_protocol::protocol_profiles() {
        let caps = profile.capacities();
        // Every decoded string byte expands to at most six JSON bytes. The
        // extra 256 bytes per entry cover field names, delimiters and numbers.
        let strings = caps.user_name_bytes + caps.command_total_bytes + caps.environment_bytes;
        let structure = 256 * (caps.command_arguments + 1 + caps.environment_entries + 1);
        let maximum = CONTROL_FRAME_HEADER_BYTES + strings * 6 + structure;
        assert!(
            maximum <= caps.max_control_frame_bytes as usize,
            "profile {:?} needs exact oversize Prepare measurement",
            profile.version()
        );
    }
}

#[tokio::test]
async fn required_features_are_checked_against_the_build_manifest() {
    let target = target::current_target().await;
    let supported = lasper_guest_protocol::protocol_profiles()
        .iter()
        .fold(FeatureSet::EMPTY, |features, profile| {
            features.union(profile.features())
        });
    GuestClient::new(
        target.clone(),
        ClientOptions {
            required_features: supported,
            ..Default::default()
        },
    )
    .unwrap();
    for bit in 0..u64::BITS {
        let feature = FeatureSet::from_bits(1 << bit);
        let result = GuestClient::new(
            target.clone(),
            ClientOptions {
                required_features: feature,
                ..Default::default()
            },
        );
        if supported.contains(feature) {
            assert!(result.is_ok(), "manifest feature {feature:?} was rejected");
        } else {
            assert_eq!(
                result
                    .err()
                    .expect("unimplemented feature was accepted")
                    .field,
                "required_features"
            );
        }
    }
}

#[tokio::test]
async fn prepare_capacity_is_checked_after_hello_before_any_prepare_byte() {
    // An escaped value makes the frame budget differ from the environment budget.
    for (frame_bytes, descriptors, expected) in [
        (4095, 4, None),
        (4096, 4, None),
        (4097, 4, Some(false)),
        (4096, 3, Some(true)),
        (4097, 3, Some(false)),
    ] {
        let mut value = String::from("\n\"\\");
        let frame_length = |value: &str| {
            let request = PrepareRequest::new(
                "root",
                ExecutionKind::Piped,
                GuestProcess::LoginShell,
                ProcessEnvironment::new([("EXTRA", value)]).unwrap(),
                None,
            )
            .unwrap();
            let mut output = vec![0; 8192];
            lasper_guest_protocol::negotiate_hello(
                HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
                HelloOffer::local(ControlLimits::CURRENT),
            )
            .unwrap()
            .encode_prepare(&request, &mut output)
            .unwrap()
                + CONTROL_FRAME_HEADER_BYTES
        };
        value.push_str(&"x".repeat(frame_bytes - frame_length(&value)));
        assert_eq!(frame_length(&value), frame_bytes);
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = [0; CONTROL_FRAME_HEADER_BYTES];
            stream.read_exact(&mut header).unwrap();
            let header = ControlFrameHeader::decode(&header).unwrap();
            assert_eq!(header.kind(), MessageKind::HelloRequest);
            let mut payload = vec![0; header.payload_bytes() as usize];
            stream.read_exact(&mut payload).unwrap();
            let request = HelloRequest::decode_payload(&payload).unwrap();
            assert_eq!(request.offer(), HelloOffer::local(request.limits()));
            let (response, _) = HelloResponse::accept(
                request,
                HelloOffer::local(ControlLimits::CURRENT),
                BrokerId::new([1; 16]),
            )
            .unwrap();
            let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
            let length = response.encode_payload(&mut bytes).unwrap();
            stream
                .write_all(
                    &ControlFrameHeader::new_with_version(
                        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
                        MessageKind::HelloResponse,
                        header.request_id(),
                        length as u32,
                        0,
                    )
                    .unwrap()
                    .encode(),
                )
                .unwrap();
            stream.write_all(&bytes[..length]).unwrap();
            if expected.is_some() {
                assert_eq!(
                    stream.read(&mut [0; 1]).unwrap(),
                    0,
                    "capacity rejection sent Prepare bytes"
                );
            } else {
                let mut header = [0; CONTROL_FRAME_HEADER_BYTES];
                stream.read_exact(&mut header).unwrap();
                let header = ControlFrameHeader::decode(&header).unwrap();
                assert_eq!(header.kind(), MessageKind::PrepareRequest);
                assert_eq!(
                    header.payload_bytes() as usize + CONTROL_FRAME_HEADER_BYTES,
                    frame_bytes
                );
                let mut payload = vec![0; header.payload_bytes() as usize];
                stream.read_exact(&mut payload).unwrap();
            }
        });
        let client = GuestClient::new(
            target::current_target().await,
            ClientOptions {
                endpoint: endpoint.to_str().unwrap().into(),
                control_limits: ControlLimits::new(4096, descriptors).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        let result = client
            .prepare(
                ProcessSpec {
                    user: "root".into(),
                    program: Program::LoginShell,
                    stdio: Stdio::Piped,
                    environment: [("EXTRA".into(), value)].into(),
                },
                Instant::now() + Duration::from_secs(3),
            )
            .await;
        match (expected, result) {
            (
                Some(false),
                Err(PrepareError::Capacity(PrepareCapacityError::FrameBytes {
                    required,
                    available,
                })),
            ) => {
                assert_eq!(required, frame_bytes);
                assert_eq!(available, 4096);
            }
            (
                Some(true),
                Err(PrepareError::Capacity(PrepareCapacityError::ReadyDescriptors {
                    required: 4,
                    available: 3,
                })),
            ) => {}
            (None, Err(PrepareError::Session { last_snapshot, .. })) => {
                assert_eq!(last_snapshot.submission, StartSubmission::NotSubmitted)
            }
            _ => panic!("unexpected Prepare capacity result"),
        }
        server.join().unwrap();
    }
}
