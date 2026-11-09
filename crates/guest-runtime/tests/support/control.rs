extern crate std;

use lasper_guest_protocol::{ControlFrameHeader, CONTROL_FRAME_HEADER_BYTES};
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::vec;
use std::vec::Vec;

pub struct Frame {
    pub header: ControlFrameHeader,
    pub payload: Vec<u8>,
    pub descriptors: Vec<OwnedFd>,
}

pub fn receive(fd: RawFd) -> io::Result<Frame> {
    let mut bytes = [0; CONTROL_FRAME_HEADER_BYTES];
    let mut control = [0usize; 64];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: The syscall borrows these two output buffers only for this call.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control);
    let count = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut descriptors = Vec::new();
    // SAFETY: recvmsg supplied complete cmsg records in this aligned buffer.
    unsafe {
        let mut record = libc::CMSG_FIRSTHDR(&message);
        while !record.is_null() {
            assert_eq!((*record).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*record).cmsg_type, libc::SCM_RIGHTS);
            let length = ((*record).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
            for i in 0..length {
                let raw = libc::CMSG_DATA(record)
                    .cast::<RawFd>()
                    .add(i)
                    .read_unaligned();
                descriptors.push(OwnedFd::from_raw_fd(raw));
            }
            record = libc::CMSG_NXTHDR(&message, record);
        }
    }
    assert_eq!(message.msg_flags & libc::MSG_CTRUNC, 0);
    let mut stream = unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map(std::fs::File::from)?;
    stream.read_exact(&mut bytes[1..])?;
    let header = ControlFrameHeader::decode(&bytes).unwrap();
    assert_eq!(header.descriptor_count() as usize, descriptors.len());
    let mut payload = vec![0; header.payload_bytes() as usize];
    stream.read_exact(&mut payload)?;
    Ok(Frame {
        header,
        payload,
        descriptors,
    })
}

pub fn send(
    fd: RawFd,
    header: ControlFrameHeader,
    payload: &[u8],
    descriptors: &[RawFd],
) -> io::Result<()> {
    let bytes = header.encode();
    let mut control = [0usize; 64];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: 1,
    };
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    if !descriptors.is_empty() {
        // SAFETY: The aligned buffer fits this small, test-controlled FD set.
        unsafe {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(size_of_val(descriptors) as u32) as usize;
            assert!(message.msg_controllen <= size_of_val(&control));
            let record = libc::CMSG_FIRSTHDR(&message);
            (*record).cmsg_len = libc::CMSG_LEN(size_of_val(descriptors) as u32) as usize;
            (*record).cmsg_level = libc::SOL_SOCKET;
            (*record).cmsg_type = libc::SCM_RIGHTS;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr(),
                libc::CMSG_DATA(record).cast(),
                descriptors.len(),
            );
        }
    }
    let count = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(count, 1);
    let mut stream = unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map(std::fs::File::from)?;
    stream.write_all(&bytes[1..])?;
    stream.write_all(payload)
}
