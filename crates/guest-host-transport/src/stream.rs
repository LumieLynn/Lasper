//! Frames control traffic and owns received descriptors before validating payloads.
//!
//! Public operations shut down the socket when cancelled. The driver's incremental
//! I/O retains partial frames so unrelated events can interrupt a poll safely.
//! Neither path verifies worker identity or enforces session operation ordering.

use lasper_guest_protocol::{
    ControlConnectionState, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError,
    ControlSessionError, CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES,
    MAX_CONTROL_DESCRIPTORS,
};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use tokio::net::UnixStream;

use crate::SCM_PIDFD;

mod handshake;
mod progress;
pub use handshake::{HandshakeError, NegotiatedHello};
use progress::ControlIo;
pub(crate) use progress::ControlIoEvent;

// Keep one overflow slot so an excessive FD count can be rejected explicitly.
const MAX_RECEIVED_DESCRIPTORS: usize = MAX_CONTROL_DESCRIPTORS as usize + 1;
const SEND_CONTROL_BYTES: usize = unsafe {
    libc::CMSG_SPACE((MAX_CONTROL_DESCRIPTORS as usize * size_of::<RawFd>()) as _) as usize
};
const RECEIVE_CONTROL_BYTES: usize =
    unsafe { libc::CMSG_SPACE((MAX_RECEIVED_DESCRIPTORS * size_of::<RawFd>()) as _) as usize };

#[repr(C, align(16))]
struct AncillaryBuffer<const BYTES: usize>([u8; BYTES]);

/// Asynchronous host transport for the guest control protocol.
///
/// Once an operation has consumed part of a frame, an error leaves no safe
/// frame boundary to resume from. The transport shuts the socket down instead
/// of risking that a frame tail is parsed as the next header.
/// The internal session driver can finish reading cleanup reports after a
/// write failure, but cannot send again or recover the stream for public use.
pub struct UnixControlStream {
    stream: UnixStream,
    poisoned: bool,
    session: ControlConnectionState,
    negotiated: Option<NegotiatedHello>,
    io: ControlIo,
}

impl UnixControlStream {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            poisoned: false,
            session: ControlConnectionState::host(),
            negotiated: None,
            io: ControlIo::default(),
        }
    }

    pub(crate) fn negotiated_hello(&self) -> Option<&NegotiatedHello> {
        if self.can_continue() {
            self.negotiated.as_ref()
        } else {
            None
        }
    }

    /// Reports local invalidation, not peer liveness. Retained partial I/O is
    /// resumable; a peer close is detected when the next I/O operation runs.
    pub fn can_continue(&self) -> bool {
        !self.poisoned && !self.io.write_failed()
    }

    /// Fails if the stream was invalidated or still has buffered input or output.
    pub fn into_inner(self) -> io::Result<UnixStream> {
        if !self.can_continue() || self.io.is_busy() {
            Err(invalid_data("control stream was interrupted mid-frame"))
        } else {
            Ok(self.stream)
        }
    }

    /// Sends one frame, with any descriptors attached to its first byte.
    /// Validation happens before I/O; a later failure poisons the stream.
    pub async fn send_frame(
        &mut self,
        header: ControlFrameHeader,
        payload: &[u8],
        descriptors: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        header
            .validate_shape(payload.len(), descriptors.len())
            .map_err(invalid_outbound_frame)?;
        let next_session = self
            .session
            .next_outbound(header)
            .map_err(invalid_outbound_session)?;

        if self.io.has_output() {
            return Err(invalid_data("a control frame is already being sent"));
        }
        let mut operation = StreamOperation::new(&self.stream, &mut self.poisoned, &mut self.io)?;
        let result = send_frame_inner(operation.stream(), header, payload, descriptors).await;
        if result.is_ok() {
            operation.finish();
            self.session = next_session;
        } else {
            operation.poison();
        }
        result
    }

    /// Cancelling shuts down the stream, including when no bytes were read.
    /// Received FDs are owned before payload validation, so a malformed frame
    /// or cancellation releases them rather than leaving them open.
    pub async fn receive_frame(&mut self) -> io::Result<ReceivedControlFrame> {
        if self.io.is_busy() {
            return Err(invalid_data("incremental control I/O is in progress"));
        }
        let session = self.session;
        let mut operation = StreamOperation::new(&self.stream, &mut self.poisoned, &mut self.io)?;
        match operation.receive_frame(session).await {
            Ok(frame) => {
                let next_session = session
                    .next_inbound(frame.header())
                    .map_err(invalid_inbound_session)?;
                operation.finish();
                self.session = next_session;
                Ok(frame)
            }
            Err(error) => {
                operation.poison();
                Err(error)
            }
        }
    }
}

/// A received frame that owns its `SCM_RIGHTS` descriptors.
#[derive(Debug)]
pub struct ReceivedControlFrame {
    header: ControlFrameHeader,
    payload: Vec<u8>,
    descriptors: Vec<OwnedFd>,
}

impl ReceivedControlFrame {
    pub const fn header(&self) -> ControlFrameHeader {
        self.header
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn descriptors(&self) -> &[OwnedFd] {
        &self.descriptors
    }

    /// Transfers ownership of every part of the received frame.
    pub fn into_parts(self) -> (ControlFrameHeader, Vec<u8>, Vec<OwnedFd>) {
        (self.header, self.payload, self.descriptors)
    }
}

struct StreamOperation<'a> {
    stream: &'a UnixStream,
    poisoned: &'a mut bool,
    io: &'a mut ControlIo,
    finished: bool,
}

impl<'a> StreamOperation<'a> {
    fn new(
        stream: &'a UnixStream,
        poisoned: &'a mut bool,
        io: &'a mut ControlIo,
    ) -> io::Result<Self> {
        if *poisoned || io.write_failed() {
            return Err(invalid_data("control stream cannot be reused"));
        }
        Ok(Self {
            stream,
            poisoned,
            io,
            finished: false,
        })
    }

    fn stream(&self) -> &UnixStream {
        self.stream
    }

    async fn receive_frame(
        &mut self,
        session: ControlConnectionState,
    ) -> io::Result<ReceivedControlFrame> {
        self.io.receive_frame(self.stream, session).await
    }

    fn finish(&mut self) {
        self.finished = true;
    }

    fn poison(&mut self) {
        *self.poisoned = true;
        shutdown_stream(self.stream);
        self.io.discard();
        self.finished = true;
    }
}

impl Drop for StreamOperation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            *self.poisoned = true;
            shutdown_stream(self.stream);
            self.io.discard();
        }
    }
}

async fn send_frame_inner(
    stream: &UnixStream,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let encoded_header = header.encode();
    let sent = sendmsg_once(stream, &encoded_header, payload, descriptors).await?;
    let frame_bytes = CONTROL_FRAME_HEADER_BYTES + payload.len();
    if sent == 0 || sent > frame_bytes {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "control frame sendmsg returned an invalid byte count",
        ));
    }

    let mut offset = sent;
    if offset < encoded_header.len() {
        write_all(stream, &encoded_header[offset..]).await?;
        offset = encoded_header.len();
    }
    let payload_offset = offset.saturating_sub(encoded_header.len());
    if payload_offset < payload.len() {
        write_all(stream, &payload[payload_offset..]).await?;
    }
    Ok(())
}

async fn write_all(stream: &UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.writable().await?;
        match stream.try_write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "control stream accepted no frame bytes",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_descriptors(descriptors: Vec<OwnedFd>, expected: u16) -> io::Result<Vec<OwnedFd>> {
    if descriptors.len() != usize::from(expected) {
        return Err(invalid_data(
            "received descriptor count does not match frame header",
        ));
    }
    Ok(descriptors)
}

fn recvmsg_once(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    if bytes.is_empty() {
        return Ok((0, Vec::new()));
    }

    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    // A zeroed header represents an empty message; the receive buffers and
    // their lengths are installed below before the syscall.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen = control.0.len() as _;

    // The byte and control buffers stay live for the call and bound every write.
    let received = unsafe {
        libc::recvmsg(
            stream.as_raw_fd(),
            &mut message,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
        )
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    let descriptors = decode_ancillary(&message, &control.0)?;
    Ok((received as usize, descriptors))
}

fn decode_ancillary(message: &libc::msghdr, control: &[u8]) -> io::Result<Vec<OwnedFd>> {
    let control_bytes: usize = message.msg_controllen as _;
    let (descriptors, unexpected_control) = collect_descriptors(message, control)?;
    if control_bytes > control.len()
        || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
    {
        return Err(invalid_data(
            "control frame or ancillary data was truncated",
        ));
    }
    if unexpected_control {
        return Err(invalid_data(
            "unexpected ancillary data on guest control stream",
        ));
    }
    if descriptors.len() > MAX_CONTROL_DESCRIPTORS as usize {
        return Err(invalid_data("too many descriptors on guest control stream"));
    }
    Ok(descriptors)
}

fn collect_descriptors(message: &libc::msghdr, control: &[u8]) -> io::Result<(Vec<OwnedFd>, bool)> {
    let mut descriptors = Vec::new();
    let mut unexpected_control = false;
    let mut rights_seen = false;
    let control_bytes: usize = message.msg_controllen as _;
    let control_bytes = control_bytes.min(control.len());
    let header_bytes = unsafe { libc::CMSG_LEN(0) } as usize;
    let mut offset = 0;

    while control_bytes.saturating_sub(offset) >= header_bytes {
        // SAFETY: The complete header fits in control; the byte slice need not be aligned.
        let header = unsafe {
            control
                .as_ptr()
                .add(offset)
                .cast::<libc::cmsghdr>()
                .read_unaligned()
        };
        let length: usize = header.cmsg_len as _;
        let data_bytes = length
            .checked_sub(header_bytes)
            .ok_or_else(|| invalid_data("malformed ancillary header"))?;
        let available_bytes = control_bytes - offset - header_bytes;
        let truncated = data_bytes > available_bytes;

        if header.cmsg_level == libc::SOL_SOCKET
            && matches!(header.cmsg_type, libc::SCM_RIGHTS | SCM_PIDFD)
        {
            if header.cmsg_type == SCM_PIDFD
                || rights_seen
                || data_bytes == 0
                || !data_bytes.is_multiple_of(size_of::<RawFd>())
            {
                unexpected_control = true;
            }
            rights_seen = true;
            // Own every complete FD slot that was delivered, even when the
            // record advertises more bytes than the receive buffer contains.
            let delivered_bytes = data_bytes.min(available_bytes);
            for index in 0..delivered_bytes / size_of::<RawFd>() {
                // SAFETY: This complete slot is bounded by the record and the
                // returned control length. Payload alignment is not assumed.
                let raw_fd = unsafe {
                    control
                        .as_ptr()
                        .add(offset + header_bytes + index * size_of::<RawFd>())
                        .cast::<RawFd>()
                        .read_unaligned()
                };
                if raw_fd >= 0 {
                    // SAFETY: Each slot in an FD-bearing receive record
                    // transfers ownership of one installed descriptor.
                    descriptors.push(unsafe { OwnedFd::from_raw_fd(raw_fd) });
                } else {
                    unexpected_control = true;
                }
            }
        } else {
            unexpected_control = true;
        }

        if truncated {
            return Err(invalid_data("ancillary record exceeds receive buffer"));
        }
        offset += unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
    }

    Ok((descriptors, unexpected_control))
}

async fn sendmsg_once(
    stream: &UnixStream,
    header: &[u8],
    payload: &[u8],
    descriptors: &[BorrowedFd<'_>],
) -> io::Result<usize> {
    if descriptors.len() > MAX_CONTROL_DESCRIPTORS as usize {
        return Err(invalid_input(
            "too many descriptors for guest control frame",
        ));
    }
    let mut raw_descriptors = [-1; MAX_CONTROL_DESCRIPTORS as usize];
    for (index, descriptor) in descriptors.iter().enumerate() {
        raw_descriptors[index] = descriptor.as_raw_fd();
    }

    loop {
        stream.writable().await?;
        match stream.try_io(tokio::io::Interest::WRITABLE, || {
            sendmsg_raw(
                stream,
                header,
                payload,
                &raw_descriptors[..descriptors.len()],
            )
        }) {
            Ok(result) => return Ok(result),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn sendmsg_raw(
    stream: &UnixStream,
    header: &[u8],
    payload: &[u8],
    descriptors: &[RawFd],
) -> io::Result<usize> {
    if descriptors.len() > MAX_CONTROL_DESCRIPTORS as usize {
        return Err(invalid_input(
            "too many descriptors for guest control frame",
        ));
    }
    let mut iov = [
        libc::iovec {
            iov_base: header.as_ptr().cast_mut().cast(),
            iov_len: header.len(),
        },
        libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        },
    ];
    let mut control = AncillaryBuffer([0; SEND_CONTROL_BYTES]);
    // A zeroed header represents an empty message; every field used by
    // `sendmsg` is filled below before the syscall.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = iov.len() as _;

    if !descriptors.is_empty() {
        let data_bytes = std::mem::size_of_val(descriptors);
        let control_bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_bytes as _;
        // The control buffer is aligned and large enough for the requested record.
        let ancillary = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if ancillary.is_null() {
            return Err(invalid_input(
                "could not allocate descriptor control record",
            ));
        }
        // `ancillary` points into the checked control allocation, and the
        // descriptor payload fits the record reserved above.
        unsafe {
            (*ancillary).cmsg_level = libc::SOL_SOCKET;
            (*ancillary).cmsg_type = libc::SCM_RIGHTS;
            (*ancillary).cmsg_len = libc::CMSG_LEN(data_bytes as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(ancillary),
                data_bytes,
            );
        }
    }

    // The kernel only borrows the live byte and control buffers for this call.
    let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

fn invalid_header(error: ControlFrameHeaderError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid guest control frame header: {error:?}"),
    )
}

fn invalid_outbound_frame(error: ControlFrameError) -> io::Error {
    invalid_input(format!("invalid outbound guest control frame: {error:?}"))
}

fn invalid_outbound_session(error: ControlSessionError) -> io::Error {
    invalid_input(format!(
        "outbound guest control frame violates the session contract: {error:?}"
    ))
}

fn invalid_inbound_session(error: ControlSessionError) -> io::Error {
    invalid_data(format!(
        "inbound guest control frame violates the session contract: {error:?}"
    ))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn shutdown_stream(stream: &UnixStream) {
    // `shutdown` borrows the live socket descriptor and never takes ownership.
    unsafe {
        libc::shutdown(stream.as_raw_fd(), libc::SHUT_RDWR);
    }
}

#[cfg(test)]
#[path = "../tests/transport/frames.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/transport/ancillary.rs"]
mod ancillary_tests;

#[cfg(test)]
#[path = "../tests/transport/progress.rs"]
mod progress_tests;
