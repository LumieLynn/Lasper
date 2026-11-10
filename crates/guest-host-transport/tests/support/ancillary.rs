use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 16]);

pub fn send_with_rights(
    stream: &UnixStream,
    bytes: &[u8],
    descriptors: &[RawFd],
) -> io::Result<usize> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; 16]);
    // SAFETY: all pointer and length fields read by sendmsg are set below.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;

    if !descriptors.is_empty() {
        let data_bytes = std::mem::size_of_val(descriptors);
        // SAFETY: this fixture sends only descriptor lists that fit the buffer.
        let control_bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        assert!(control_bytes <= size_of::<AncillaryBuffer>());
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_bytes as _;
        // SAFETY: msg_control points to aligned storage of msg_controllen bytes.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        // SAFETY: the checked allocation holds both this header and the FD list.
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
    }

    loop {
        // SAFETY: the byte and control buffers remain live throughout sendmsg.
        let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
        if written >= 0 {
            return Ok(written as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
