use lasper_guest_protocol::ControlFrameHeader;
use lasper_guest_runtime::transport::{GuestControlStream, GuestReceivedFrame, TransportError};
use std::task::Poll;
use std::time::{Duration, Instant};

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

pub(crate) fn drive<T>(
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
