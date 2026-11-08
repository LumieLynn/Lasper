use crate::linux::uapi::{file::F_DUPFD_CLOEXEC, nr, socket::SHUT_RDWR};
use crate::syscall::{syscall1, syscall2, syscall3};
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};

// These layouts follow the 64-bit Linux syscall ABI, independently of the
// C library used by the guest.
#[repr(C)]
pub(crate) struct Iovec {
    pub(crate) base: *mut c_void,
    pub(crate) length: usize,
}

const _: () = {
    assert!(size_of::<Iovec>() == 16);
    assert!(align_of::<Iovec>() == 8);
    assert!(offset_of!(Iovec, base) == 0);
    assert!(offset_of!(Iovec, length) == 8);
};

#[repr(C)]
pub(crate) struct MessageHeader {
    pub(crate) name: *mut c_void,
    pub(crate) name_length: u32,
    pub(crate) vectors: *mut Iovec,
    pub(crate) vector_count: usize,
    pub(crate) control: *mut c_void,
    pub(crate) control_length: usize,
    pub(crate) flags: u32,
}

const _: () = {
    assert!(size_of::<MessageHeader>() == 56);
    assert!(align_of::<MessageHeader>() == 8);
    assert!(offset_of!(MessageHeader, name) == 0);
    assert!(offset_of!(MessageHeader, name_length) == 8);
    assert!(offset_of!(MessageHeader, vectors) == 16);
    assert!(offset_of!(MessageHeader, vector_count) == 24);
    assert!(offset_of!(MessageHeader, control) == 32);
    assert!(offset_of!(MessageHeader, control_length) == 40);
    assert!(offset_of!(MessageHeader, flags) == 48);
};

#[repr(C)]
pub(crate) struct ControlMessageHeader {
    pub(crate) length: usize,
    pub(crate) level: i32,
    pub(crate) kind: i32,
}

const _: () = {
    assert!(size_of::<ControlMessageHeader>() == 16);
    assert!(align_of::<ControlMessageHeader>() == 8);
    assert!(offset_of!(ControlMessageHeader, length) == 0);
    assert!(offset_of!(ControlMessageHeader, level) == 8);
    assert!(offset_of!(ControlMessageHeader, kind) == 12);
};

pub(crate) const fn cmsg_align(length: usize) -> usize {
    (length + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

pub(crate) const fn cmsg_space(data_bytes: usize) -> usize {
    cmsg_align(size_of::<ControlMessageHeader>()) + cmsg_align(data_bytes)
}

pub(crate) const fn cmsg_len(data_bytes: usize) -> usize {
    cmsg_align(size_of::<ControlMessageHeader>()) + data_bytes
}

pub(crate) unsafe fn cmsg_data(header: *mut ControlMessageHeader) -> *mut u8 {
    unsafe {
        header
            .cast::<u8>()
            .add(cmsg_align(size_of::<ControlMessageHeader>()))
    }
}

pub(crate) fn sendmsg(fd: i32, message: &MessageHeader, flags: usize) -> isize {
    unsafe {
        syscall3(
            nr::SENDMSG,
            fd as usize,
            message as *const _ as usize,
            flags,
        )
    }
}

pub(crate) fn recvmsg(fd: i32, message: &mut MessageHeader, flags: usize) -> isize {
    unsafe { syscall3(nr::RECVMSG, fd as usize, message as *mut _ as usize, flags) }
}

pub(crate) fn shutdown(fd: i32) -> isize {
    unsafe { syscall2(nr::SHUTDOWN, fd as usize, SHUT_RDWR) }
}

pub(crate) fn close(fd: i32) -> isize {
    unsafe { syscall1(nr::CLOSE, fd as usize) }
}

pub(crate) fn duplicate_cloexec(fd: i32) -> Result<i32, i32> {
    let result = unsafe { syscall3(nr::FCNTL, fd as usize, F_DUPFD_CLOEXEC, 3) };
    if result < 0 {
        Err((-result) as i32)
    } else {
        Ok(result as i32)
    }
}
