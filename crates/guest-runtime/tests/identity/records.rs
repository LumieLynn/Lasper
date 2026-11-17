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
