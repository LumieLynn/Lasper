extern crate std;
use super::*;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn incomplete_record_closes_all_complete_descriptor_slots() {
    for kind in [SCM_RIGHTS, SCM_PIDFD] {
        for trailing_bytes in 0..size_of::<i32>() {
            let (received, mut peer) = UnixStream::pair().unwrap();
            let mut control = AncillaryBuffer([0; 16]);
            // SAFETY: The buffer holds the header and one owned descriptor.
            // Its advertised payload is deliberately longer than the supplied bytes.
            unsafe {
                let header = control.0.as_mut_ptr().cast::<ControlMessageHeader>();
                (*header).length = cmsg_align(size_of::<ControlMessageHeader>()) + 8;
                (*header).level = SOL_SOCKET;
                (*header).kind = kind;
                header
                    .cast::<u8>()
                    .add(cmsg_align(size_of::<ControlMessageHeader>()))
                    .cast::<i32>()
                    .write_unaligned(received.into_raw_fd());
            }
            // SAFETY: The parser only reads control_length from this header.
            let mut message: MessageHeader = unsafe { zeroed() };
            message.control_length =
                cmsg_align(size_of::<ControlMessageHeader>()) + size_of::<i32>() + trailing_bytes;

            assert_eq!(
                discard_ancillary(&message, &control),
                Err(IdentityTransportError::Truncated)
            );
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }
}
