//! Private datagrams carrying a small status/request and at most four FDs.
//! CLOEXEC is applied by recvmsg itself, before another host thread can exec.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

pub(super) fn receive(
    socket: BorrowedFd<'_>,
    payload: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    // cmsghdr supplies the alignment required for walking ancillary records.
    let mut control: [libc::cmsghdr; 3] = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(4 * std::mem::size_of::<i32>() as u32) } as _;
    let bytes = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut descriptors = Vec::new();
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        // Only the other end of our socketpair can send these frames. Own all
        // delivered descriptors even when the packet was truncated.
        unsafe {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let length =
                    ((*header).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let data = libc::CMSG_DATA(header).cast::<i32>();
                for index in 0..length / std::mem::size_of::<i32>() {
                    descriptors.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated guest bus connector packet",
        ));
    }
    Ok((bytes as usize, descriptors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendfd::SendWithFd;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn transferred_descriptors_are_owned_and_close_on_exec() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        sender.send_with_fd(b"ok", &[file.as_raw_fd()]).unwrap();
        let (count, fds) = receive(receiver.as_fd(), &mut [0; 8]).unwrap();
        assert_eq!(count, 2);
        assert_eq!(fds.len(), 1);
        assert_ne!(
            unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
