use lasper_guest_protocol::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub fn protocol() -> NegotiatedProtocol {
    negotiate_hello(
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloOffer::local(ControlLimits::CURRENT),
    )
    .unwrap()
}

pub fn hello(stream: &mut UnixStream) -> BrokerId {
    let request = HelloRequest::new(FeatureSet::WORKER_PIDFD, ControlLimits::CURRENT).unwrap();
    let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = request.encode_payload(&mut bytes).unwrap();
    let header = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::HelloRequest,
        1,
        length as u32,
        0,
    )
    .unwrap();
    stream.write_all(&header.encode()).unwrap();
    stream.write_all(&bytes[..length]).unwrap();
    let mut header = [0; CONTROL_FRAME_HEADER_BYTES];
    stream.read_exact(&mut header).unwrap();
    let header = ControlFrameHeader::decode(&header).unwrap();
    assert_eq!(header.kind(), MessageKind::HelloResponse);
    stream
        .read_exact(&mut bytes[..header.payload_bytes() as usize])
        .unwrap();
    let response =
        HelloResponse::decode_payload(&bytes[..header.payload_bytes() as usize]).unwrap();
    request
        .negotiate_protocol(header.protocol_version(), response)
        .unwrap();
    response.broker_id()
}

pub fn request(stream: &UnixStream, broker: BrokerId) -> PrepareRequestFrame<'static> {
    request_command(stream, broker, &["/bin/sh", "-c", "printf broker-output"])
}

pub fn request_command(
    stream: &UnixStream,
    broker: BrokerId,
    argv: &[&'static str],
) -> PrepareRequestFrame<'static> {
    request_with_environment(stream, broker, argv, ProcessEnvironment::default())
}

pub fn request_with_environment<'a>(
    stream: &UnixStream,
    broker: BrokerId,
    argv: &[&'a str],
    environment: ProcessEnvironment<'a>,
) -> PrepareRequestFrame<'a> {
    let request = PrepareRequestFrame::new(
        10,
        broker,
        PrepareRequest::new(
            "alice",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            environment,
            None,
        )
        .unwrap(),
    );
    let mut payload = vec![0; MAX_CONTROL_FRAME_BYTES as usize];
    let length = protocol()
        .encode_prepare_frame(&request, &mut payload)
        .unwrap();
    let header = ControlFrameHeader::decode(&payload[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    super::wire::send(
        stream.as_raw_fd(),
        header,
        &payload[CONTROL_FRAME_HEADER_BYTES..length],
        &[],
    )
    .unwrap();
    request
}

pub fn ready(
    stream: &UnixStream,
    request: &PrepareRequestFrame<'_>,
) -> (PrepareReadyFrame, Vec<std::os::fd::OwnedFd>) {
    let frame = super::wire::receive(stream.as_raw_fd()).unwrap();
    assert_eq!(
        frame.header.kind(),
        MessageKind::PrepareReady,
        "Prepare rejected: {}",
        String::from_utf8_lossy(&frame.payload)
    );
    let ready = protocol()
        .decode_ready(
            request,
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    (ready, frame.descriptors)
}

pub fn start(stream: &UnixStream, request: &PrepareRequestFrame<'_>, ready: &PrepareReadyFrame) {
    let start = StartFrame::new(
        11,
        StartRequest::new(request, ready.response().preparation_id(), None, None).unwrap(),
    );
    let mut payload = [0; 4096];
    let (header, length) = protocol().encode_start_frame(&start, &mut payload).unwrap();
    super::wire::send(stream.as_raw_fd(), header, &payload[..length], &[]).unwrap();
    let frame = super::wire::receive(stream.as_raw_fd()).unwrap();
    protocol()
        .decode_start_accepted(
            start.request_id(),
            request,
            ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
}

pub fn finish(stream: &UnixStream, request: &PrepareRequestFrame<'_>, ready: &PrepareReadyFrame) {
    loop {
        let frame = super::wire::receive(stream.as_raw_fd()).unwrap();
        let exit = protocol()
            .decode_exit(
                request,
                ready.response().preparation_id(),
                frame.header,
                &frame.payload,
                frame.descriptors.len(),
            )
            .unwrap();
        if exit.report().is_final() {
            assert_eq!(exit.report().exit(), Some(ExitStatus::Exited(0)));
            return;
        }
    }
}
