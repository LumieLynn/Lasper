use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind, CONTROL_FRAME_HEADER_BYTES,
};
use lasper_guest_runtime::transport::{GuestControlStream, GuestReceivedFrame, TransportError};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::task::Poll;
use std::time::{Duration, Instant};

pub fn stream_pair() -> (GuestControlStream, UnixStream) {
    let (guest, peer) = UnixStream::pair().unwrap();
    // SAFETY: Ownership transfers out of UnixStream; no second owner remains.
    (
        unsafe { GuestControlStream::from_raw_fd(guest.into_raw_fd()) },
        peer,
    )
}

pub fn established(limits: ControlLimits) -> (GuestControlStream, UnixStream) {
    let (mut guest, mut peer) = stream_pair();
    peer.write_all(&header(MessageKind::HelloRequest, 0, 0).encode())
        .unwrap();
    let mut buffer = [];
    receive(&guest, &mut buffer).unwrap();
    send(&guest, header(MessageKind::HelloResponse, 0, 0), &[], &[]).unwrap();
    let mut response = [0; CONTROL_FRAME_HEADER_BYTES];
    peer.read_exact(&mut response).unwrap();
    let limits = HelloRequest::new(FeatureSet::EMPTY, limits)
        .unwrap()
        .negotiate(
            lasper_guest_protocol::PROTOCOL_MAJOR,
            HelloResponse::new(FeatureSet::EMPTY, BrokerId::new([1; 16]), limits).unwrap(),
        )
        .unwrap();
    guest.complete_hello(limits).unwrap();
    (guest, peer)
}

pub fn header(kind: MessageKind, payload_bytes: usize, fds: u16) -> ControlFrameHeader {
    ControlFrameHeader::new(kind, 17, payload_bytes as u32, fds).unwrap()
}

pub fn receive<'a>(
    stream: &GuestControlStream,
    buffer: &'a mut [u8],
) -> Result<GuestReceivedFrame<&'a mut [u8]>, TransportError> {
    let mut header = stream.begin_receive()?;
    drive(stream.as_raw_fd(), READABLE, || header.advance())?;
    let mut body = header.into_payload(buffer)?;
    drive(stream.as_raw_fd(), READABLE, || body.advance())?;
    body.finish()
}

pub fn send(
    stream: &GuestControlStream,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[i32],
) -> Result<(), TransportError> {
    let mut writer = stream.begin_send(header, payload, descriptors)?;
    drive(stream.as_raw_fd(), WRITABLE, || writer.advance())
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
            Poll::Pending => wait(fd, events, deadline),
        }
    }
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    returned: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
}
const READABLE: i16 = 1;
const WRITABLE: i16 = 4;

fn wait(fd: i32, events: i16, deadline: Instant) {
    let mut entry = PollFd {
        fd,
        events,
        returned: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "transport made no progress before its test deadline"
        );
        // SAFETY: entry is one writable pollfd, alive for the whole syscall.
        let result = unsafe {
            poll(
                &mut entry,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if result > 0 {
            return;
        }
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        panic!(
            "transport poll failed or timed out: {:?}",
            std::io::Error::last_os_error()
        );
    }
}

pub fn small_send_buffer(fd: i32) {
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const i32, length: u32) -> i32;
    }
    let bytes = 4096;
    // SAFETY: setsockopt reads the integer only for this call.
    assert_eq!(
        unsafe { setsockopt(fd, 1, 7, &bytes, size_of::<i32>() as u32) },
        0
    );
}

pub fn assert_closed(mut peer: UnixStream) {
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

pub fn raw_fd(stream: &UnixStream) -> i32 {
    stream.as_raw_fd()
}
