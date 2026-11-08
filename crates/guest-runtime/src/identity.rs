use core::mem::{size_of, zeroed};
use lasper_guest_protocol::{
    GuestIdentityReply, IdentityChallenge, IdentityChannelError, PayloadCodecError, PreparationId,
    MAX_IDENTITY_MESSAGE_BYTES,
};

use crate::socket::{
    close, cmsg_align, recvmsg, sendmsg, shutdown, ControlMessageHeader, Iovec, MessageHeader,
    EINTR, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_NOSIGNAL, MSG_TRUNC, SCM_PIDFD, SCM_RIGHTS,
    SOL_SOCKET,
};

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityTransportError {
    Payload(IdentityChannelError),
    Syscall(i32),
    UnexpectedEof,
    Truncated,
    UnexpectedAncillary,
    WriteZero,
    UnexpectedPhase,
    PreparationMismatch,
    ChallengeMismatch,
    Poisoned,
}

/// The worker receives each challenge after switching to its target account.
/// Replies carry no explicit credentials: SO_PASSCRED on the host endpoint
/// makes the kernel attach the sender's credentials to each packet.
pub struct GuestIdentityChannel {
    fd: i32,
    preparation_id: PreparationId,
    pending: Option<IdentityChallenge>,
    poisoned: bool,
}

impl GuestIdentityChannel {
    /// `preparation_id` comes from the guardian's worker assignment, before any
    /// challenge is received. It cannot be learned from the first packet.
    ///
    /// # Safety
    ///
    /// `fd` must be an exclusively owned, connected AF_UNIX/SOCK_SEQPACKET
    /// descriptor in blocking mode. This channel takes ownership of it.
    pub unsafe fn from_raw_fd(fd: i32, preparation_id: PreparationId) -> Self {
        Self {
            fd,
            preparation_id,
            pending: None,
            poisoned: false,
        }
    }

    pub const fn can_continue(&self) -> bool {
        !self.poisoned
    }

    pub(crate) const fn as_raw_fd(&self) -> i32 {
        self.fd
    }

    /// A buffer too small for the received packet discards that packet and
    /// invalidates the channel; seqpacket has no unread tail to recover.
    pub fn receive_challenge(
        &mut self,
        packet: &mut [u8],
    ) -> Result<IdentityChallenge, IdentityTransportError> {
        self.ensure_usable()?;
        if self.pending.is_some() {
            return Err(IdentityTransportError::UnexpectedPhase);
        }
        if packet.is_empty() {
            return Err(IdentityTransportError::Payload(
                IdentityChannelError::Payload(PayloadCodecError::OutputTooSmall),
            ));
        }
        let capacity = packet.len().min(MAX_IDENTITY_MESSAGE_BYTES);
        let length = match receive_packet(self.fd, &mut packet[..capacity]) {
            Ok(length) => length,
            Err(error) => return self.fail(error),
        };
        let challenge = match IdentityChallenge::decode_packet(&packet[..length]) {
            Ok(challenge) => challenge,
            Err(error) => return self.fail(IdentityTransportError::Payload(error)),
        };
        if challenge.preparation_id() != self.preparation_id {
            return self.fail(IdentityTransportError::PreparationMismatch);
        }
        self.pending = Some(challenge);
        Ok(challenge)
    }

    /// Encoding or challenge errors leave the pending challenge intact. Once
    /// sendmsg fails, the channel is shut down instead of retrying a reply.
    pub fn send_reply(
        &mut self,
        reply: &GuestIdentityReply,
        packet: &mut [u8],
    ) -> Result<(), IdentityTransportError> {
        self.ensure_usable()?;
        let challenge = self
            .pending
            .ok_or(IdentityTransportError::UnexpectedPhase)?;
        if reply.preparation_id() != challenge.preparation_id()
            || reply.challenge() != challenge.nonce()
        {
            return Err(IdentityTransportError::ChallengeMismatch);
        }
        let length = reply
            .encode_packet(packet)
            .map_err(IdentityTransportError::Payload)?;
        if let Err(error) = send_packet(self.fd, &packet[..length]) {
            return self.fail(error);
        }
        self.pending = None;
        Ok(())
    }

    fn ensure_usable(&self) -> Result<(), IdentityTransportError> {
        if self.poisoned {
            Err(IdentityTransportError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn fail<T>(&mut self, error: IdentityTransportError) -> Result<T, IdentityTransportError> {
        self.poisoned = true;
        let _ = shutdown(self.fd);
        Err(error)
    }
}

impl Drop for GuestIdentityChannel {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

fn send_packet(fd: i32, packet: &[u8]) -> Result<(), IdentityTransportError> {
    let mut vector = Iovec {
        base: packet.as_ptr().cast_mut().cast(),
        length: packet.len(),
    };
    // The unused name and control pointers must remain null.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = &mut vector;
    message.vector_count = 1;

    loop {
        let result = sendmsg(fd, &message, MSG_NOSIGNAL);
        if result == -EINTR {
            continue;
        }
        if result < 0 {
            return Err(IdentityTransportError::Syscall((-result) as i32));
        }
        // A continuation send would be a second packet, not this packet's tail.
        if result as usize != packet.len() {
            return Err(IdentityTransportError::WriteZero);
        }
        return Ok(());
    }
}

fn receive_packet(fd: i32, packet: &mut [u8]) -> Result<usize, IdentityTransportError> {
    let mut vector = Iovec {
        base: packet.as_mut_ptr().cast(),
        length: packet.len(),
    };
    let mut control = AncillaryBuffer([0; 16]);
    // recvmsg writes only to the live buffers installed below.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = &mut vector;
    message.vector_count = 1;
    message.control = control.0.as_mut_ptr().cast();

    let received = loop {
        message.control_length = size_of::<AncillaryBuffer>();
        message.flags = 0;
        let result = recvmsg(fd, &mut message, MSG_CMSG_CLOEXEC);
        if result == -EINTR {
            continue;
        }
        if result < 0 {
            return Err(IdentityTransportError::Syscall((-result) as i32));
        }
        break result as usize;
    };

    // Even a truncated packet may have installed rights. Close those before
    // returning any truncation, ancillary or payload error.
    let ancillary = discard_ancillary(&message, &control);
    if message.flags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
        return Err(IdentityTransportError::Truncated);
    }
    ancillary?;
    if received == 0 {
        return Err(IdentityTransportError::UnexpectedEof);
    }
    Ok(received)
}

fn discard_ancillary(
    message: &MessageHeader,
    control: &AncillaryBuffer,
) -> Result<(), IdentityTransportError> {
    let length = message.control_length.min(size_of::<AncillaryBuffer>());
    let header_bytes = cmsg_align(size_of::<ControlMessageHeader>());
    let mut offset = 0;
    while length.saturating_sub(offset) >= header_bytes {
        // offset is inside the returned range and covers a complete header.
        let header = unsafe {
            control
                .0
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<ControlMessageHeader>()
                .read_unaligned()
        };
        let data_bytes = header
            .length
            .checked_sub(header_bytes)
            .ok_or(IdentityTransportError::Truncated)?;
        let available_bytes = length - offset - header_bytes;
        let truncated = data_bytes > available_bytes;
        if header.level == SOL_SOCKET && matches!(header.kind, SCM_RIGHTS | SCM_PIDFD) {
            // Rejected identity packets still own any delivered FD slots,
            // including the complete prefix of a truncated record.
            for index in 0..data_bytes.min(available_bytes) / size_of::<i32>() {
                // SAFETY: This complete slot fits both the record and the buffer.
                let fd = unsafe {
                    control
                        .0
                        .as_ptr()
                        .cast::<u8>()
                        .add(offset + header_bytes + index * size_of::<i32>())
                        .cast::<i32>()
                        .read_unaligned()
                };
                if fd >= 0 {
                    let _ = close(fd);
                }
            }
        }
        if truncated {
            return Err(IdentityTransportError::Truncated);
        }
        offset += cmsg_align(header.length);
    }
    if message.control_length > size_of::<AncillaryBuffer>() {
        Err(IdentityTransportError::Truncated)
    } else if length != 0 {
        Err(IdentityTransportError::UnexpectedAncillary)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn empty_rights_are_rejected() {
        let mut control = AncillaryBuffer([0; 16]);
        // SAFETY: The first aligned record fits this local control allocation.
        unsafe {
            let header = control.0.as_mut_ptr().cast::<ControlMessageHeader>();
            (*header).length = size_of::<ControlMessageHeader>();
            (*header).level = SOL_SOCKET;
            (*header).kind = SCM_RIGHTS;
        }
        // SAFETY: The parser uses only control_length from this local header.
        let mut message: MessageHeader = unsafe { zeroed() };
        message.control_length = size_of::<ControlMessageHeader>();
        assert_eq!(
            discard_ancillary(&message, &control),
            Err(IdentityTransportError::UnexpectedAncillary)
        );
    }

    #[test]
    fn unexpected_pidfd_is_closed() {
        let (received, mut peer) = UnixStream::pair().unwrap();
        let mut control = AncillaryBuffer([0; 16]);
        // SAFETY: This local allocation holds the complete record. Detaching
        // the FD simulates recvmsg transferring its ownership to the receiver.
        unsafe {
            let header = control.0.as_mut_ptr().cast::<ControlMessageHeader>();
            (*header).length = size_of::<ControlMessageHeader>() + size_of::<i32>();
            (*header).level = SOL_SOCKET;
            (*header).kind = SCM_PIDFD;
            header
                .cast::<u8>()
                .add(size_of::<ControlMessageHeader>())
                .cast::<i32>()
                .write_unaligned(received.into_raw_fd());
        }
        // SAFETY: The parser only reads control_length from the message header.
        let mut message: MessageHeader = unsafe { zeroed() };
        message.control_length = cmsg_align(size_of::<ControlMessageHeader>() + size_of::<i32>());
        assert_eq!(
            discard_ancillary(&message, &control),
            Err(IdentityTransportError::UnexpectedAncillary)
        );
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn unexpected_control_does_not_skip_later_rights() {
        let (received, mut peer) = UnixStream::pair().unwrap();
        let fd = received.into_raw_fd();
        let mut control = AncillaryBuffer([0; 16]);
        let first_record_bytes = size_of::<ControlMessageHeader>();
        let second_record_bytes = first_record_bytes + size_of::<i32>();
        // SAFETY: Both records fit the local control allocation. The FD was
        // deliberately detached above to simulate recvmsg transferring ownership.
        unsafe {
            let first = control.0.as_mut_ptr().cast::<ControlMessageHeader>();
            (*first).length = first_record_bytes;
            (*first).level = SOL_SOCKET;
            (*first).kind = 2;
            let second = control
                .0
                .as_mut_ptr()
                .cast::<u8>()
                .add(first_record_bytes)
                .cast::<ControlMessageHeader>();
            (*second).length = second_record_bytes;
            (*second).level = SOL_SOCKET;
            (*second).kind = SCM_RIGHTS;
            second
                .cast::<u8>()
                .add(first_record_bytes)
                .cast::<i32>()
                .write_unaligned(fd);
        }
        // SAFETY: The parser uses only control_length from this local header.
        let mut message: MessageHeader = unsafe { zeroed() };
        message.control_length = first_record_bytes + cmsg_align(second_record_bytes);
        assert_eq!(
            discard_ancillary(&message, &control),
            Err(IdentityTransportError::UnexpectedAncillary)
        );
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[cfg(test)]
#[path = "../tests/identity/ancillary.rs"]
mod ancillary_tests;
