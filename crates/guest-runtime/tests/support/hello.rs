extern crate std;

use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind, CONTROL_FRAME_HEADER_BYTES, HELLO_PAYLOAD_BUFFER_BYTES,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn exchange(control: &mut UnixStream) {
    control
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    control
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let request = HelloRequest::new(FeatureSet::WORKER_PIDFD, ControlLimits::CURRENT).unwrap();
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = request.encode_payload(&mut payload).unwrap();
    let header = ControlFrameHeader::new(MessageKind::HelloRequest, 1, length as u32, 0).unwrap();
    control.write_all(&header.encode()).unwrap();
    control.write_all(&payload[..length]).unwrap();
    let mut header = [0; CONTROL_FRAME_HEADER_BYTES];
    control.read_exact(&mut header).unwrap();
    let header = ControlFrameHeader::decode(&header).unwrap();
    assert_eq!(header.kind(), MessageKind::HelloResponse);
    assert_eq!(header.request_id(), 1);
    control
        .read_exact(&mut payload[..header.payload_bytes() as usize])
        .unwrap();
    let response =
        HelloResponse::decode_payload(&payload[..header.payload_bytes() as usize]).unwrap();
    request
        .negotiate(header.protocol_version(), response)
        .unwrap();
    assert_eq!(response.broker_id(), BrokerId::new([1; 16]));
}
