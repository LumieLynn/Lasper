use lasper_guest_protocol::{
    ControlFrameError, ControlFrameHeader, ControlFrameHeaderError,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use tokio::net::UnixStream;

const MAX_RECEIVED_DESCRIPTORS: usize = MAX_CONTROL_DESCRIPTORS as usize + 1;

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 16]);

/// Asynchronous host transport for the guest control protocol.
///
/// Once an operation has consumed part of a frame, an error leaves no safe
/// frame boundary to resume from. The transport shuts the socket down instead
/// of risking that a frame tail is parsed as the next header.
pub struct UnixControlStream {
    stream: UnixStream,
    poisoned: bool,
}

impl UnixControlStream {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            poisoned: false,
        }
    }

    /// Returns true while the stream is still aligned to a frame boundary.
    pub fn can_continue(&self) -> bool {
        !self.poisoned
    }

    /// Returns the socket only while its framing state is still known.
    pub fn into_inner(self) -> io::Result<UnixStream> {
        if self.poisoned {
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

        let (stream, poisoned) = (&self.stream, &mut self.poisoned);
        let mut operation = StreamOperation::new(stream, poisoned)?;
        let result = send_frame_inner(operation.stream(), header, payload, descriptors).await;
        if result.is_ok() {
            operation.finish();
        } else {
            operation.poison();
        }
        result
    }

    /// Receives one frame and takes ownership of its descriptors immediately.
    ///
    /// Descriptor ownership is established before the rest of the frame is
    /// validated, so cancellation or malformed input cannot leak them.
    pub async fn receive_frame(&mut self) -> io::Result<ReceivedControlFrame> {
        let (stream, poisoned) = (&self.stream, &mut self.poisoned);
        let mut operation = StreamOperation::new(stream, poisoned)?;
        let result = receive_frame_inner(operation.stream()).await;
        if result.is_ok() {
            operation.finish();
        } else {
            operation.poison();
        }
        result
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
    finished: bool,
}

impl<'a> StreamOperation<'a> {
    fn new(stream: &'a UnixStream, poisoned: &'a mut bool) -> io::Result<Self> {
        if *poisoned {
            return Err(invalid_data("control stream cannot be reused"));
        }
        Ok(Self {
            stream,
            poisoned,
            finished: false,
        })
    }

    fn stream(&self) -> &UnixStream {
        self.stream
    }

    fn finish(&mut self) {
        self.finished = true;
    }

    fn poison(&mut self) {
        *self.poisoned = true;
        shutdown_stream(self.stream);
        self.finished = true;
    }
}

impl Drop for StreamOperation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            *self.poisoned = true;
            shutdown_stream(self.stream);
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

async fn receive_frame_inner(stream: &UnixStream) -> io::Result<ReceivedControlFrame> {
    // `SCM_RIGHTS` is tied to a byte position on a stream socket. Reading the
    // anchor alone prevents rights from a later byte being accepted by this frame.
    let mut first = [0; CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES];
    let (read, descriptors) = receive_chunk(stream, &mut first).await?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control stream closed before a frame began",
        ));
    }

    let mut encoded_header = [0; CONTROL_FRAME_HEADER_BYTES];
    encoded_header[..CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES].copy_from_slice(&first);
    read_exact_without_descriptors(
        stream,
        &mut encoded_header[CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES..],
    )
    .await?;
    let header = ControlFrameHeader::decode(&encoded_header).map_err(invalid_header)?;
    let descriptors = validate_descriptors(descriptors, header.descriptor_count())?;

    let mut payload = vec![0; header.payload_bytes() as usize];
    read_exact_without_descriptors(stream, &mut payload).await?;
    Ok(ReceivedControlFrame {
        header,
        payload,
        descriptors,
    })
}

async fn read_exact_without_descriptors(
    stream: &UnixStream,
    mut bytes: &mut [u8],
) -> io::Result<()> {
    while !bytes.is_empty() {
        let (read, descriptors) = receive_chunk(stream, bytes).await?;
        if !descriptors.is_empty() {
            return Err(invalid_data(
                "control frame descriptors were not attached to its first byte",
            ));
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "control stream closed before the frame was complete",
            ));
        }
        bytes = &mut bytes[read..];
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

async fn receive_chunk(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    loop {
        stream.readable().await?;
        match stream.try_io(tokio::io::Interest::READABLE, || {
            recvmsg_once(stream, bytes)
        }) {
            Ok(result) => return Ok(result),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn recvmsg_once(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    if bytes.is_empty() {
        return Ok((0, Vec::new()));
    }

    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; 16]);
    // A zeroed header represents an empty message; the receive buffers and
    // their lengths are installed below before the syscall.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE((MAX_RECEIVED_DESCRIPTORS * size_of::<RawFd>()) as _) as usize };

    // The byte and control buffers stay live for the call and bound every write.
    let received =
        unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    let descriptors = decode_ancillary(&mut message, &control)?;
    Ok((received as usize, descriptors))
}

fn decode_ancillary(
    message: &mut libc::msghdr,
    control: &AncillaryBuffer,
) -> io::Result<Vec<OwnedFd>> {
    let control_overflow = message.msg_controllen > size_of::<AncillaryBuffer>();
    if control_overflow {
        message.msg_controllen = size_of::<AncillaryBuffer>();
    }
    let (descriptors, unexpected_control) = collect_descriptors(message, control)?;
    if control_overflow || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
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

fn collect_descriptors(
    message: &libc::msghdr,
    control: &AncillaryBuffer,
) -> io::Result<(Vec<OwnedFd>, bool)> {
    let mut descriptors = Vec::new();
    let mut unexpected_control = false;
    let mut rights_seen = false;
    let control_start = control.0.as_ptr() as usize;
    let control_end = control_start.saturating_add(message.msg_controllen);
    // `msg_control` points at `control`, and its returned length fits the allocation.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };

    while !header.is_null() {
        // Each libc cursor is checked against the returned control range before
        // its record is interpreted.
        let (level, kind, length) = unsafe {
            (
                (*header).cmsg_level,
                (*header).cmsg_type,
                (*header).cmsg_len as usize,
            )
        };
        let header_start = header as usize;
        let data_start = header_start.saturating_add(unsafe { libc::CMSG_LEN(0) } as usize);
        let data_length = length
            .checked_sub(data_start.saturating_sub(header_start))
            .ok_or_else(|| invalid_data("malformed ancillary header"))?;
        let record_end = header_start
            .checked_add(length)
            .ok_or_else(|| invalid_data("malformed ancillary length"))?;
        if header_start < control_start || record_end > control_end {
            return Err(invalid_data("ancillary record exceeds receive buffer"));
        }

        if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
            if rights_seen || data_length == 0 || data_length % size_of::<RawFd>() != 0 {
                unexpected_control = true;
            }
            rights_seen = true;
            // The complete `SCM_RIGHTS` payload lies inside the checked record.
            let data = unsafe { libc::CMSG_DATA(header).cast::<RawFd>() };
            for index in 0..data_length / size_of::<RawFd>() {
                // This slot lies inside the checked descriptor payload.
                let raw_fd = unsafe { data.add(index).read_unaligned() };
                // `SCM_RIGHTS` installs a new descriptor; `OwnedFd` assumes its
                // ownership here so every later error closes it.
                descriptors.push(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        } else {
            unexpected_control = true;
        }

        // libc computes the next cursor; the next iteration validates its record.
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
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
    if descriptors.len() > (size_of::<AncillaryBuffer>() / size_of::<RawFd>()) {
        return Err(invalid_input(
            "descriptor control record exceeds local buffer",
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
    let mut control = AncillaryBuffer([0; 16]);
    // A zeroed header represents an empty message; every field used by
    // `sendmsg` is filled below before the syscall.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = iov.len();

    if !descriptors.is_empty() {
        let data_bytes = std::mem::size_of_val(descriptors);
        let control_bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        if control_bytes > size_of::<AncillaryBuffer>() {
            return Err(invalid_input(
                "descriptor control record exceeds local buffer",
            ));
        }
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_bytes;
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
mod tests {
    use super::*;
    use lasper_guest_protocol::MessageKind;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsFd, IntoRawFd};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{timeout, Duration};

    fn test_header(payload_bytes: usize, descriptor_count: u16) -> ControlFrameHeader {
        ControlFrameHeader::new(
            MessageKind::PrepareRequest,
            17,
            payload_bytes as u32,
            descriptor_count,
        )
        .unwrap()
    }

    fn rights_message(control: &mut AncillaryBuffer, descriptors: &[RawFd]) -> libc::msghdr {
        // The test fills the only control fields read by the parser below.
        let mut message: libc::msghdr = unsafe { zeroed() };
        let data_bytes = std::mem::size_of_val(descriptors);
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        // The test buffer is aligned and sized for the supplied descriptors.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        // `header` points into the checked test control allocation.
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(data_bytes as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                data_bytes,
            );
        }
        message
    }

    fn assert_peer_observes_close(mut peer: StdUnixStream) {
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn empty_rights_record_is_rejected() {
        let mut control = AncillaryBuffer([0; 16]);
        let mut message = rights_message(&mut control, &[]);

        let error = decode_ancillary(&mut message, &control).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_control_closes_descriptors_before_returning() {
        let (received, peer) = StdUnixStream::pair().unwrap();
        let fd = received.into_raw_fd();
        let mut control = AncillaryBuffer([0; 16]);
        let mut message = rights_message(&mut control, &[fd]);
        message.msg_flags = libc::MSG_CTRUNC;

        let error = decode_ancillary(&mut message, &control).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_peer_observes_close(peer);
    }

    #[test]
    fn oversized_control_length_closes_descriptors_before_returning() {
        let (received, peer) = StdUnixStream::pair().unwrap();
        let fd = received.into_raw_fd();
        let mut control = AncillaryBuffer([0; 16]);
        let mut message = rights_message(&mut control, &[fd]);
        message.msg_controllen = size_of::<AncillaryBuffer>() + 1;

        let error = decode_ancillary(&mut message, &control).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_peer_observes_close(peer);
    }

    #[tokio::test]
    async fn frames_round_trip_and_pass_owned_close_on_exec_descriptors() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut sender = UnixControlStream::new(sender);
        let mut receiver = UnixControlStream::new(receiver);
        let file = File::open("/dev/null").unwrap();
        let payload = b"prepare";
        let header = test_header(payload.len(), 1);
        let descriptors = [file.as_fd()];

        let (sent, frame) = tokio::join!(
            sender.send_frame(header, payload, &descriptors),
            receiver.receive_frame()
        );
        sent.unwrap();
        let frame = frame.unwrap();

        assert_eq!(frame.header(), header);
        assert_eq!(frame.payload(), payload);
        assert_eq!(frame.descriptors().len(), 1);
        assert_ne!(
            unsafe { libc::fcntl(frame.descriptors()[0].as_raw_fd(), libc::F_GETFD) }
                & libc::FD_CLOEXEC,
            0
        );
    }

    #[tokio::test]
    async fn large_frame_short_writes_keep_rights_attached_once() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let small_send_buffer: libc::c_int = 4096;
        let result = unsafe {
            libc::setsockopt(
                sender.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&small_send_buffer as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as _,
            )
        };
        assert_eq!(result, 0);

        let mut sender = UnixControlStream::new(sender);
        let mut receiver = UnixControlStream::new(receiver);
        let file = File::open("/dev/null").unwrap();
        let payload = vec![b'x'; 512 * 1024];
        let header = test_header(payload.len(), 1);
        let descriptors = [file.as_fd()];
        let (sent, frame) = tokio::join!(
            sender.send_frame(header, &payload, &descriptors),
            receiver.receive_frame()
        );

        sent.unwrap();
        let frame = frame.unwrap();
        assert_eq!(frame.payload(), payload);
        assert_eq!(frame.descriptors().len(), 1);
        assert!(sender.can_continue());
        assert!(receiver.can_continue());
    }

    #[tokio::test]
    async fn coalesced_frames_are_received_individually() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut sender = UnixControlStream::new(sender);
        let mut receiver = UnixControlStream::new(receiver);
        let first = test_header(5, 0);
        let second = test_header(6, 0);

        sender.send_frame(first, b"first", &[]).await.unwrap();
        sender.send_frame(second, b"second", &[]).await.unwrap();

        assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"first");
        assert_eq!(receiver.receive_frame().await.unwrap().payload(), b"second");
    }

    #[tokio::test]
    async fn descriptor_count_mismatch_rejects_and_poison_closes_the_stream() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = UnixControlStream::new(receiver);
        let file = File::open("/dev/null").unwrap();
        let header = test_header(0, 0).encode();

        let written = sendmsg_once(&sender, &header, &[], &[file.as_fd()])
            .await
            .unwrap();
        assert_eq!(written, header.len());
        let error = receiver.receive_frame().await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!receiver.can_continue());
        assert!(receiver.receive_frame().await.is_err());
    }

    #[tokio::test]
    async fn descriptors_attached_after_the_first_frame_byte_are_rejected() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = UnixControlStream::new(receiver);
        let file = File::open("/dev/null").unwrap();
        let header = test_header(1, 0).encode();
        sender.write_all(&header).await.unwrap();
        sendmsg_once(&sender, &[], b"x", &[file.as_fd()])
            .await
            .unwrap();

        let error = receiver.receive_frame().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!receiver.can_continue());
    }

    #[tokio::test]
    async fn truncated_rights_are_rejected() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = UnixControlStream::new(receiver);
        let (transferred, peers): (Vec<_>, Vec<_>) =
            (0..5).map(|_| StdUnixStream::pair().unwrap()).unzip();
        let raw_fds = transferred
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        let header = test_header(0, 2).encode();

        let written = sendmsg_raw(&sender, &header, &[], &raw_fds).unwrap();
        assert_eq!(written, header.len());
        drop(transferred);
        let error = receiver.receive_frame().await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!receiver.can_continue());
        for peer in peers {
            assert_peer_observes_close(peer);
        }
    }

    #[tokio::test]
    async fn cancelling_a_partial_frame_receive_invalidates_the_stream() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = UnixControlStream::new(receiver);
        sender.write_all(&[0x4c]).await.unwrap();

        let result = timeout(Duration::from_millis(20), receiver.receive_frame()).await;

        assert!(result.is_err());
        assert!(!receiver.can_continue());
        assert!(receiver.into_inner().is_err());
    }

    #[tokio::test]
    async fn healthy_stream_can_return_its_inner_socket() {
        let (stream, _peer) = UnixStream::pair().unwrap();

        assert!(UnixControlStream::new(stream).into_inner().is_ok());
    }

    #[tokio::test]
    async fn outbound_shape_errors_do_not_touch_the_stream() {
        let (sender, _receiver) = UnixStream::pair().unwrap();
        let mut sender = UnixControlStream::new(sender);
        let file = File::open("/dev/null").unwrap();

        let error = sender
            .send_frame(test_header(0, 0), b"not-empty", &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(sender.can_continue());

        let error = sender
            .send_frame(test_header(0, 0), &[], &[file.as_fd()])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(sender.can_continue());
    }
}
