use super::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest,
    HelloResponse, MessageKind,
};
use lasper_guest_runtime::transport::GuestControlStream;
use std::os::unix::net::UnixStream as StdUnixStream;
use tokio::net::UnixStream;

fn header(kind: MessageKind, payload: &[u8], descriptors: u16) -> ControlFrameHeader {
    ControlFrameHeader::new(kind, 17, payload.len() as u32, descriptors).unwrap()
}

pub fn raw_stream_pair() -> (UnixControlStream, StdUnixStream) {
    let (host, guest) = StdUnixStream::pair().unwrap();
    host.set_nonblocking(true).unwrap();
    (
        UnixControlStream::new(UnixStream::from_std(host).unwrap()),
        guest,
    )
}

pub fn current_limits() -> EnforcedControlLimits {
    HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT)
        .unwrap()
        .negotiate(
            lasper_guest_protocol::ProtocolVersion::CURRENT,
            HelloResponse::new(
                FeatureSet::EMPTY,
                BrokerId::new([0x5a; 16]),
                ControlLimits::CURRENT,
            )
            .unwrap(),
        )
        .unwrap()
}

pub async fn establish_host(host: &mut UnixControlStream) {
    host.send_frame(header(MessageKind::HelloRequest, &[], 0), &[], &[])
        .await
        .unwrap();
    let response = host.receive_frame().await.unwrap();
    assert_eq!(response.header().kind(), MessageKind::HelloResponse);
    host.complete_hello(current_limits()).unwrap();
}

pub fn establish_guest(guest: &mut GuestControlStream) {
    let mut payload = [];
    let request = receive_guest_frame(guest, &mut payload).unwrap();
    assert_eq!(request.header().kind(), MessageKind::HelloRequest);
    send_guest_frame(
        guest,
        ControlFrameHeader::new(
            MessageKind::HelloResponse,
            request.header().request_id(),
            0,
            0,
        )
        .unwrap(),
        &[],
        &[],
    )
    .unwrap();
    guest.complete_hello(current_limits()).unwrap();
}
