use core::arch::asm;
use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr;
use lasper_guest_protocol::{
    ControlFrameError, ControlFrameHeader, ControlFrameHeaderError,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
};

const MAX_RECEIVED_DESCRIPTORS: usize = MAX_CONTROL_DESCRIPTORS as usize + 1;
const INVALID_FD: i32 = -1;
const EINTR: isize = 4;
const MSG_CTRUNC: u32 = 8;
const MSG_TRUNC: u32 = 32;
const MSG_NOSIGNAL: usize = 0x4000;
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
const SCM_RIGHTS: i32 = 1;
const SOL_SOCKET: i32 = 1;
const SHUT_RDWR: usize = 2;

#[repr(C)]
struct Iovec {
    base: *mut c_void,
    length: usize,
}

#[repr(C)]
struct MessageHeader {
    name: *mut c_void,
    name_length: u32,
    vectors: *mut Iovec,
    vector_count: usize,
    control: *mut c_void,
    control_length: usize,
    flags: u32,
}

#[repr(C)]
struct ControlMessageHeader {
    length: usize,
    level: i32,
    kind: i32,
}

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 4]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    InvalidFrame(ControlFrameError),
    InvalidHeader(ControlFrameHeaderError),
    PayloadBufferTooSmall { required: usize, available: usize },
    DescriptorCountMismatch { declared: u16, actual: usize },
    UnexpectedDescriptors,
    Truncated,
    UnexpectedEof,
    WriteZero,
    Syscall(i32),
    Poisoned,
}

/// Blocking control transport used by the guest helper.
///
/// Once an operation has consumed part of a frame, an error leaves no safe
/// frame boundary to resume from. The transport shuts the socket down instead
/// of risking that a frame tail is parsed as the next header.
pub struct GuestControlStream {
    fd: i32,
    poisoned: bool,
}

impl GuestControlStream {
    /// Takes ownership of a connected AF_UNIX stream descriptor.
    ///
    /// # Safety
    ///
    /// `fd` must be a live descriptor owned by the caller and must not be used
    /// elsewhere after this call.
    pub unsafe fn from_raw_fd(fd: i32) -> Self {
        Self {
            fd,
            poisoned: false,
        }
    }

    /// Returns true while the stream is still aligned to a frame boundary.
    pub const fn can_continue(&self) -> bool {
        !self.poisoned
    }

    /// Sends one frame, with any descriptors attached to its first byte.
    /// Validation happens before I/O; a later failure poisons the stream.
    pub fn send_frame(
        &mut self,
        header: ControlFrameHeader,
        payload: &[u8],
        descriptors: &[i32],
    ) -> Result<(), TransportError> {
        self.ensure_usable()?;
        header
            .validate_shape(payload.len(), descriptors.len())
            .map_err(TransportError::InvalidFrame)?;

        let result = send_frame(self.fd, header, payload, descriptors);
        if result.is_err() {
            self.poison();
        }
        result
    }

    /// Receives one frame into caller-provided payload storage.
    ///
    /// Descriptor ownership is established before the rest of the frame is
    /// validated, so malformed input cannot leak descriptors into the helper.
    pub fn receive_frame<'a>(
        &mut self,
        payload: &'a mut [u8],
    ) -> Result<GuestReceivedFrame<'a>, TransportError> {
        self.ensure_usable()?;
        let result = receive_frame(self.fd, payload);
        if result.is_err() {
            self.poison();
        }
        result
    }

    fn ensure_usable(&self) -> Result<(), TransportError> {
        if self.poisoned {
            Err(TransportError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn poison(&mut self) {
        self.poisoned = true;
        let _ = shutdown(self.fd);
    }
}

impl Drop for GuestControlStream {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

/// A frame that borrows the caller's payload buffer and owns its descriptors.
pub struct GuestReceivedFrame<'a> {
    header: ControlFrameHeader,
    payload: &'a [u8],
    descriptors: ReceivedDescriptors,
}

impl<'a> GuestReceivedFrame<'a> {
    pub const fn header(&self) -> ControlFrameHeader {
        self.header
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn descriptors(&self) -> &ReceivedDescriptors {
        &self.descriptors
    }

    /// Transfers ownership of the received descriptors.
    pub fn into_parts(self) -> (ControlFrameHeader, &'a [u8], ReceivedDescriptors) {
        (self.header, self.payload, self.descriptors)
    }
}

/// Owns descriptors installed by `SCM_RIGHTS`. Dropping it closes them.
pub struct ReceivedDescriptors {
    raw: [i32; MAX_CONTROL_DESCRIPTORS as usize],
    count: usize,
}

impl ReceivedDescriptors {
    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn as_raw_fds(&self) -> &[i32] {
        &self.raw[..self.count]
    }

    /// Transfers ownership to the caller. Only the first returned `count`
    /// entries are valid and must eventually be closed.
    pub fn into_raw_fds(mut self) -> ([i32; MAX_CONTROL_DESCRIPTORS as usize], usize) {
        let raw = self.raw;
        let count = self.count;
        self.count = 0;
        self.raw = [INVALID_FD; MAX_CONTROL_DESCRIPTORS as usize];
        (raw, count)
    }
}

impl Drop for ReceivedDescriptors {
    fn drop(&mut self) {
        for fd in &self.raw[..self.count] {
            let _ = close(*fd);
        }
    }
}

struct RawDescriptors {
    raw: [i32; MAX_RECEIVED_DESCRIPTORS],
    count: usize,
}

impl RawDescriptors {
    const fn new() -> Self {
        Self {
            raw: [INVALID_FD; MAX_RECEIVED_DESCRIPTORS],
            count: 0,
        }
    }

    fn push(&mut self, fd: i32) {
        if self.count < self.raw.len() {
            self.raw[self.count] = fd;
            self.count += 1;
        } else {
            let _ = close(fd);
        }
    }

    fn finish(mut self, expected: u16) -> Result<ReceivedDescriptors, TransportError> {
        if self.count != expected as usize {
            return Err(TransportError::DescriptorCountMismatch {
                declared: expected,
                actual: self.count,
            });
        }
        let mut result = ReceivedDescriptors {
            raw: [INVALID_FD; MAX_CONTROL_DESCRIPTORS as usize],
            count: self.count,
        };
        for index in 0..self.count {
            result.raw[index] = self.raw[index];
            self.raw[index] = INVALID_FD;
        }
        self.count = 0;
        Ok(result)
    }
}

impl Drop for RawDescriptors {
    fn drop(&mut self) {
        for fd in &self.raw[..self.count] {
            let _ = close(*fd);
        }
    }
}

fn send_frame(
    fd: i32,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[i32],
) -> Result<(), TransportError> {
    let mut send = send_message;
    send_frame_with(fd, header, payload, descriptors, &mut send)
}

fn send_frame_with<F>(
    fd: i32,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[i32],
    send: &mut F,
) -> Result<(), TransportError>
where
    F: FnMut(i32, &[u8], &[u8], &[i32]) -> Result<usize, TransportError>,
{
    let encoded_header = header.encode();
    let sent = send(fd, &encoded_header, payload, descriptors)?;
    let frame_bytes = encoded_header.len() + payload.len();
    if sent == 0 || sent > frame_bytes {
        return Err(TransportError::WriteZero);
    }

    let mut offset = sent;
    if offset < encoded_header.len() {
        send_all_with(fd, &encoded_header[offset..], send)?;
        offset = encoded_header.len();
    }
    let payload_offset = offset.saturating_sub(encoded_header.len());
    if payload_offset < payload.len() {
        send_all_with(fd, &payload[payload_offset..], send)?;
    }
    Ok(())
}

fn receive_frame<'a>(
    fd: i32,
    payload: &'a mut [u8],
) -> Result<GuestReceivedFrame<'a>, TransportError> {
    let mut first = [0; CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES];
    let (read, descriptors) = receive_chunk(fd, &mut first)?;
    if read == 0 {
        return Err(TransportError::UnexpectedEof);
    }

    let mut encoded_header = [0; CONTROL_FRAME_HEADER_BYTES];
    encoded_header[..CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES].copy_from_slice(&first);
    receive_exact_without_descriptors(
        fd,
        &mut encoded_header[CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES..],
    )?;
    let header =
        ControlFrameHeader::decode(&encoded_header).map_err(TransportError::InvalidHeader)?;
    let required = header.payload_bytes() as usize;
    if payload.len() < required {
        return Err(TransportError::PayloadBufferTooSmall {
            required,
            available: payload.len(),
        });
    }
    let descriptors = descriptors.finish(header.descriptor_count())?;
    receive_exact_without_descriptors(fd, &mut payload[..required])?;

    Ok(GuestReceivedFrame {
        header,
        payload: &payload[..required],
        descriptors,
    })
}

fn receive_exact_without_descriptors(fd: i32, mut bytes: &mut [u8]) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        let (read, descriptors) = receive_chunk(fd, bytes)?;
        if descriptors.count != 0 {
            return Err(TransportError::UnexpectedDescriptors);
        }
        if read == 0 {
            return Err(TransportError::UnexpectedEof);
        }
        bytes = &mut bytes[read..];
    }
    Ok(())
}

fn send_all_with<F>(fd: i32, mut bytes: &[u8], send: &mut F) -> Result<(), TransportError>
where
    F: FnMut(i32, &[u8], &[u8], &[i32]) -> Result<usize, TransportError>,
{
    while !bytes.is_empty() {
        let written = send(fd, bytes, &[], &[])?;
        if written == 0 {
            return Err(TransportError::WriteZero);
        }
        if written > bytes.len() {
            return Err(TransportError::WriteZero);
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn send_message(
    fd: i32,
    first: &[u8],
    second: &[u8],
    descriptors: &[i32],
) -> Result<usize, TransportError> {
    let mut vectors = [
        Iovec {
            base: ptr::null_mut(),
            length: 0,
        },
        Iovec {
            base: ptr::null_mut(),
            length: 0,
        },
    ];
    let mut vector_count = 0;
    for bytes in [first, second] {
        if !bytes.is_empty() {
            vectors[vector_count] = Iovec {
                base: bytes.as_ptr().cast_mut().cast(),
                length: bytes.len(),
            };
            vector_count += 1;
        }
    }
    let mut control = AncillaryBuffer([0; 4]);
    // A zeroed header represents an empty message; every field used by
    // `sendmsg` is filled below before the syscall.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = vectors.as_mut_ptr();
    message.vector_count = vector_count;

    if !descriptors.is_empty() {
        let data_bytes = core::mem::size_of_val(descriptors);
        let control_bytes = cmsg_space(data_bytes);
        if control_bytes > size_of::<AncillaryBuffer>() {
            return Err(TransportError::DescriptorCountMismatch {
                declared: MAX_CONTROL_DESCRIPTORS,
                actual: descriptors.len(),
            });
        }
        message.control = control.0.as_mut_ptr().cast();
        message.control_length = control_bytes;
        let header = message.control.cast::<ControlMessageHeader>();
        // The local control buffer is aligned and the record size was checked.
        unsafe {
            (*header).length = cmsg_len(data_bytes);
            (*header).level = SOL_SOCKET;
            (*header).kind = SCM_RIGHTS;
            ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                cmsg_data(header),
                data_bytes,
            );
        }
    }

    loop {
        let result = sendmsg(fd, &message, MSG_NOSIGNAL);
        if result == -EINTR {
            continue;
        }
        return syscall_result(result);
    }
}

fn receive_chunk(fd: i32, bytes: &mut [u8]) -> Result<(usize, RawDescriptors), TransportError> {
    let mut vector = Iovec {
        base: bytes.as_mut_ptr().cast(),
        length: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; 4]);
    // A zeroed header represents an empty message; the receive buffers and
    // their lengths are installed below before the syscall.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = &mut vector;
    message.vector_count = 1;
    message.control = control.0.as_mut_ptr().cast();
    message.control_length = cmsg_space(MAX_RECEIVED_DESCRIPTORS * size_of::<i32>());

    let received = loop {
        let result = recvmsg(fd, &mut message, MSG_CMSG_CLOEXEC);
        if result == -EINTR {
            continue;
        }
        break syscall_result(result)?;
    };

    let descriptors = decode_ancillary(&mut message, &control)?;
    Ok((received, descriptors))
}

fn decode_ancillary(
    message: &mut MessageHeader,
    control: &AncillaryBuffer,
) -> Result<RawDescriptors, TransportError> {
    let control_overflow = message.control_length > size_of::<AncillaryBuffer>();
    if control_overflow {
        message.control_length = size_of::<AncillaryBuffer>();
    }
    let descriptors = parse_descriptors(message, control)?;
    if control_overflow || message.flags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
        return Err(TransportError::Truncated);
    }
    Ok(descriptors)
}

fn parse_descriptors(
    message: &MessageHeader,
    control: &AncillaryBuffer,
) -> Result<RawDescriptors, TransportError> {
    let mut descriptors = RawDescriptors::new();
    let start = control.0.as_ptr() as usize;
    let end = start.saturating_add(message.control_length);
    let mut current = if message.control_length >= size_of::<ControlMessageHeader>() {
        message.control.cast::<ControlMessageHeader>()
    } else {
        ptr::null_mut()
    };
    let mut rights_seen = false;
    let mut unexpected_control = false;

    while !current.is_null() {
        // Each record is kept inside the returned control range before its
        // payload is interpreted.
        let (length, level, kind) =
            unsafe { ((*current).length, (*current).level, (*current).kind) };
        let current_start = current as usize;
        let data_start =
            current_start.saturating_add(cmsg_align(size_of::<ControlMessageHeader>()));
        let record_end = current_start
            .checked_add(length)
            .ok_or(TransportError::Truncated)?;
        if current_start < start
            || length < cmsg_align(size_of::<ControlMessageHeader>())
            || record_end > end
        {
            return Err(TransportError::Truncated);
        }
        if level == SOL_SOCKET && kind == SCM_RIGHTS {
            if rights_seen {
                unexpected_control = true;
            }
            rights_seen = true;
            let data_bytes = length - cmsg_align(size_of::<ControlMessageHeader>());
            if data_bytes == 0 || !data_bytes.is_multiple_of(size_of::<i32>()) {
                unexpected_control = true;
            }
            let data = data_start as *const i32;
            for index in 0..data_bytes / size_of::<i32>() {
                // The record bounds above cover this complete descriptor slot.
                descriptors.push(unsafe { data.add(index).read_unaligned() });
            }
        } else {
            unexpected_control = true;
        }

        let next = current_start.saturating_add(cmsg_align(length));
        current = if next.saturating_add(size_of::<ControlMessageHeader>()) <= end {
            next as *mut ControlMessageHeader
        } else {
            ptr::null_mut()
        };
    }

    if unexpected_control {
        Err(TransportError::UnexpectedDescriptors)
    } else {
        Ok(descriptors)
    }
}

const fn cmsg_align(length: usize) -> usize {
    (length + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

const fn cmsg_space(data_bytes: usize) -> usize {
    cmsg_align(size_of::<ControlMessageHeader>()) + cmsg_align(data_bytes)
}

const fn cmsg_len(data_bytes: usize) -> usize {
    cmsg_align(size_of::<ControlMessageHeader>()) + data_bytes
}

unsafe fn cmsg_data(header: *mut ControlMessageHeader) -> *mut u8 {
    unsafe {
        header
            .cast::<u8>()
            .add(cmsg_align(size_of::<ControlMessageHeader>()))
    }
}

fn syscall_result(result: isize) -> Result<usize, TransportError> {
    if result < 0 {
        Err(TransportError::Syscall((-result) as i32))
    } else {
        Ok(result as usize)
    }
}

fn sendmsg(fd: i32, message: &MessageHeader, flags: usize) -> isize {
    unsafe {
        syscall3(
            SYS_SENDMSG,
            fd as usize,
            message as *const _ as usize,
            flags,
        )
    }
}

fn recvmsg(fd: i32, message: &mut MessageHeader, flags: usize) -> isize {
    unsafe { syscall3(SYS_RECVMSG, fd as usize, message as *mut _ as usize, flags) }
}

fn shutdown(fd: i32) -> isize {
    unsafe { syscall2(SYS_SHUTDOWN, fd as usize, SHUT_RDWR) }
}

fn close(fd: i32) -> isize {
    unsafe { syscall1(SYS_CLOSE, fd as usize) }
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall1(number: usize, first: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall2(number: usize, first: usize, second: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall3(number: usize, first: usize, second: usize, third: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall1(number: usize, first: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall2(number: usize, first: usize, second: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall3(number: usize, first: usize, second: usize, third: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x2") third,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
const SYS_CLOSE: usize = 3;
#[cfg(target_arch = "x86_64")]
const SYS_SENDMSG: usize = 46;
#[cfg(target_arch = "x86_64")]
const SYS_RECVMSG: usize = 47;
#[cfg(target_arch = "x86_64")]
const SYS_SHUTDOWN: usize = 48;

#[cfg(target_arch = "aarch64")]
const SYS_CLOSE: usize = 57;
#[cfg(target_arch = "aarch64")]
const SYS_SHUTDOWN: usize = 210;
#[cfg(target_arch = "aarch64")]
const SYS_SENDMSG: usize = 211;
#[cfg(target_arch = "aarch64")]
const SYS_RECVMSG: usize = 212;

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;

    fn rights_message(control: &mut AncillaryBuffer, descriptors: &[i32]) -> MessageHeader {
        // The test fills the only control fields read by the parser below.
        let mut message: MessageHeader = unsafe { zeroed() };
        let data_bytes = core::mem::size_of_val(descriptors);
        message.control = control.0.as_mut_ptr().cast();
        message.control_length = cmsg_space(data_bytes);
        let header = message.control.cast::<ControlMessageHeader>();
        // The test buffer is aligned and sized for the supplied descriptors.
        unsafe {
            (*header).length = cmsg_len(data_bytes);
            (*header).level = SOL_SOCKET;
            (*header).kind = SCM_RIGHTS;
            ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                cmsg_data(header),
                data_bytes,
            );
        }
        message
    }

    fn assert_peer_observes_close(mut peer: UnixStream) {
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn empty_rights_record_is_rejected() {
        let mut control = AncillaryBuffer([0; 4]);
        let mut message = rights_message(&mut control, &[]);

        assert!(matches!(
            decode_ancillary(&mut message, &control),
            Err(TransportError::UnexpectedDescriptors)
        ));
    }

    #[test]
    fn truncated_control_closes_descriptors_before_returning() {
        let (received, peer) = UnixStream::pair().unwrap();
        let fd = received.into_raw_fd();
        let mut control = AncillaryBuffer([0; 4]);
        let mut message = rights_message(&mut control, &[fd]);
        message.flags = MSG_CTRUNC;

        assert!(matches!(
            decode_ancillary(&mut message, &control),
            Err(TransportError::Truncated)
        ));
        assert_peer_observes_close(peer);
    }

    #[test]
    fn oversized_control_length_closes_descriptors_before_returning() {
        let (received, peer) = UnixStream::pair().unwrap();
        let fd = received.into_raw_fd();
        let mut control = AncillaryBuffer([0; 4]);
        let mut message = rights_message(&mut control, &[fd]);
        message.control_length = size_of::<AncillaryBuffer>() + 1;

        assert!(matches!(
            decode_ancillary(&mut message, &control),
            Err(TransportError::Truncated)
        ));
        assert_peer_observes_close(peer);
    }

    #[test]
    fn large_frame_retries_short_writes_without_repeating_descriptors() {
        let payload = std::vec![0x5a; 256 * 1024];
        let header = ControlFrameHeader::new(
            lasper_guest_protocol::MessageKind::PrepareRequest,
            31,
            payload.len() as u32,
            2,
        )
        .unwrap();
        let descriptors = [41, 42];
        let mut written = std::vec::Vec::new();
        let mut calls = std::vec::Vec::new();
        let mut send = |_: i32, first: &[u8], second: &[u8], fds: &[i32]| {
            let available = first.len() + second.len();
            let accepted = available.min(97);
            let first_bytes = accepted.min(first.len());
            written.extend_from_slice(&first[..first_bytes]);
            written.extend_from_slice(&second[..accepted - first_bytes]);
            calls.push(fds.to_vec());
            Ok(accepted)
        };

        send_frame_with(7, header, &payload, &descriptors, &mut send).unwrap();

        let mut expected = header.encode().to_vec();
        expected.extend_from_slice(&payload);
        assert_eq!(written, expected);
        assert_eq!(calls.first().unwrap(), &descriptors);
        assert!(calls.iter().skip(1).all(std::vec::Vec::is_empty));
        assert!(calls.len() > 2);
    }
}
