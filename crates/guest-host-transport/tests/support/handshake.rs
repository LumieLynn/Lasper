use super::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::raw::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind,
};
use lasper_guest_runtime::transport::GuestControlStream;
use std::os::unix::net::UnixStream as StdUnixStream;
use tokio::net::UnixStream;

pub fn response_for(
    request: HelloRequest,
    features: FeatureSet,
    broker: BrokerId,
    limits: ControlLimits,
) -> Result<HelloResponse, lasper_guest_protocol::CompatibilityError> {
    HelloResponse::accept(
        request,
        lasper_guest_protocol::HelloOffer {
            supported_features: features,
            ..lasper_guest_protocol::HelloOffer::local(limits)
        },
        broker,
    )
    .map(|(response, _)| response)
}

pub fn raw_stream_pair() -> (UnixControlStream, StdUnixStream) {
    let (host, guest) = StdUnixStream::pair().unwrap();
    host.set_nonblocking(true).unwrap();
    (
        UnixControlStream::new(UnixStream::from_std(host).unwrap()),
        guest,
    )
}

pub async fn establish_host(host: &mut UnixControlStream) {
    host.handshake(
        17,
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
    )
    .await
    .unwrap();
}

pub fn establish_guest(
    guest: &mut GuestControlStream,
) -> lasper_guest_protocol::NegotiatedProtocol {
    let mut payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let frame = receive_guest_frame(guest, &mut payload).unwrap();
    assert_eq!(frame.header().kind(), MessageKind::HelloRequest);
    let request_id = frame.header().request_id();
    let request = HelloRequest::decode_payload(frame.payload()).unwrap();
    let (response, negotiated) = HelloResponse::accept(
        request,
        lasper_guest_protocol::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([0x5a; 16]),
    )
    .unwrap();
    let length = response.encode_payload(&mut payload).unwrap();
    send_guest_frame(
        guest,
        ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
            MessageKind::HelloResponse,
            request_id,
            length as u32,
            0,
        )
        .unwrap(),
        &payload[..length],
        &[],
    )
    .unwrap();
    guest.complete_protocol(negotiated).unwrap();
    negotiated
}
