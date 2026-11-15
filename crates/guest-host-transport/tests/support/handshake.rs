use super::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind,
};
use lasper_guest_runtime::transport::GuestControlStream;
use std::os::unix::net::UnixStream as StdUnixStream;
use tokio::net::UnixStream;

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

pub fn establish_guest(guest: &mut GuestControlStream) {
    let mut payload = [0; lasper_guest_protocol::HELLO_PAYLOAD_BUFFER_BYTES];
    let frame = receive_guest_frame(guest, &mut payload).unwrap();
    assert_eq!(frame.header().kind(), MessageKind::HelloRequest);
    let request_id = frame.header().request_id();
    let request = HelloRequest::decode_payload(frame.payload()).unwrap();
    let response = HelloResponse::new(
        FeatureSet::EMPTY,
        BrokerId::new([0x5a; 16]),
        ControlLimits::CURRENT,
    )
    .unwrap();
    let limits = request
        .negotiate(lasper_guest_protocol::ProtocolVersion::CURRENT, response)
        .unwrap();
    let length = response.encode_payload(&mut payload).unwrap();
    send_guest_frame(
        guest,
        ControlFrameHeader::new(MessageKind::HelloResponse, request_id, length as u32, 0).unwrap(),
        &payload[..length],
        &[],
    )
    .unwrap();
    guest.complete_hello(limits).unwrap();
}
