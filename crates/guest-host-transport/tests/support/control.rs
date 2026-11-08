use lasper_guest_host_transport::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest,
    HelloResponse, MessageKind,
};
use lasper_guest_runtime::transport::{GuestControlStream, GuestReceivedFrame, TransportError};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::task::Poll;
use std::time::{Duration, Instant};
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
            lasper_guest_protocol::PROTOCOL_MAJOR,
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

pub fn receive_guest_frame<'a>(
    guest: &GuestControlStream,
    payload: &'a mut [u8],
) -> Result<GuestReceivedFrame<&'a mut [u8]>, TransportError> {
    let mut header = guest.begin_receive()?;
    drive(guest.as_raw_fd(), libc::POLLIN, || header.advance())?;
    let mut body = header.into_payload(payload)?;
    drive(guest.as_raw_fd(), libc::POLLIN, || body.advance())?;
    body.finish()
}

pub fn send_guest_frame(
    guest: &GuestControlStream,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[i32],
) -> Result<(), TransportError> {
    let mut writer = guest.begin_send(header, payload, descriptors)?;
    drive(guest.as_raw_fd(), libc::POLLOUT, || writer.advance())
}

fn drive<T>(
    fd: i32,
    events: i16,
    mut advance: impl FnMut() -> Result<Poll<T>, TransportError>,
) -> Result<T, TransportError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match advance()? {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => {
                let mut entry = libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                };
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "guest transport timed out");
                    // SAFETY: entry is the only pollfd and remains writable throughout poll.
                    let result = unsafe {
                        libc::poll(
                            &mut entry,
                            1,
                            remaining.as_millis().min(i32::MAX as u128) as i32,
                        )
                    };
                    if result > 0 {
                        break;
                    }
                    if result < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    panic!("guest transport poll failed or timed out");
                }
            }
        }
    }
}
