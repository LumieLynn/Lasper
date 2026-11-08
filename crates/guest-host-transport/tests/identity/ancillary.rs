use super::*;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn incomplete_record_closes_all_complete_descriptor_slots() {
    for kind in [libc::SCM_RIGHTS, SCM_PIDFD] {
        for pass_pidfd in [false, true] {
            for trailing_bytes in 0..size_of::<RawFd>() {
                let (received, mut peer) = UnixStream::pair().unwrap();
                let mut control = AncillaryBuffer([0; 16]);
                // SAFETY: The buffer holds the header and one owned descriptor.
                // Its advertised payload is deliberately longer than the supplied bytes.
                unsafe {
                    let header = control.0.as_mut_ptr().cast::<libc::cmsghdr>();
                    (*header).cmsg_len = libc::CMSG_LEN(8) as _;
                    (*header).cmsg_level = libc::SOL_SOCKET;
                    (*header).cmsg_type = kind;
                    libc::CMSG_DATA(header)
                        .cast::<RawFd>()
                        .write_unaligned(received.into_raw_fd());
                }
                // SAFETY: The parser only reads msg_controllen from this header.
                let mut message: libc::msghdr = unsafe { zeroed() };
                message.msg_controllen = (unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) }
                    as usize
                    + trailing_bytes) as _;

                assert!(parse_credentials(&message, &control, pass_pidfd).is_err());
                peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                assert_eq!(peer.read(&mut [0]).unwrap(), 0);
            }
        }
    }
}
