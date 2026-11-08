use crate::syscall::{syscall1, syscall2, syscall3};
use core::ffi::c_void;
use core::mem::size_of;

pub(crate) const EINTR: isize = 4;
pub(crate) const MSG_CTRUNC: u32 = 8;
pub(crate) const MSG_TRUNC: u32 = 32;
pub(crate) const MSG_DONTWAIT: usize = 0x40;
pub(crate) const MSG_NOSIGNAL: usize = 0x4000;
pub(crate) const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
pub(crate) const SCM_RIGHTS: i32 = 1;
pub(crate) const SCM_PIDFD: i32 = 4;
pub(crate) const SOL_SOCKET: i32 = 1;
const SHUT_RDWR: usize = 2;

// These layouts follow the 64-bit Linux syscall ABI, independently of the
// C library used by the guest.
#[repr(C)]
pub(crate) struct Iovec {
    pub(crate) base: *mut c_void,
    pub(crate) length: usize,
}

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

#[repr(C)]
pub(crate) struct ControlMessageHeader {
    pub(crate) length: usize,
    pub(crate) level: i32,
    pub(crate) kind: i32,
}

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
            SYS_SENDMSG,
            fd as usize,
            message as *const _ as usize,
            flags,
        )
    }
}

pub(crate) fn recvmsg(fd: i32, message: &mut MessageHeader, flags: usize) -> isize {
    unsafe { syscall3(SYS_RECVMSG, fd as usize, message as *mut _ as usize, flags) }
}

pub(crate) fn shutdown(fd: i32) -> isize {
    unsafe { syscall2(SYS_SHUTDOWN, fd as usize, SHUT_RDWR) }
}

pub(crate) fn close(fd: i32) -> isize {
    unsafe { syscall1(SYS_CLOSE, fd as usize) }
}

pub(crate) fn duplicate_cloexec(fd: i32) -> Result<i32, i32> {
    const F_DUPFD_CLOEXEC: usize = 1030;
    let result = unsafe { syscall3(SYS_FCNTL, fd as usize, F_DUPFD_CLOEXEC, 3) };
    if result < 0 {
        Err((-result) as i32)
    } else {
        Ok(result as i32)
    }
}

#[cfg(target_arch = "x86_64")]
const SYS_CLOSE: usize = 3;
#[cfg(target_arch = "x86_64")]
const SYS_SENDMSG: usize = 46;
#[cfg(target_arch = "x86_64")]
const SYS_RECVMSG: usize = 47;
#[cfg(target_arch = "x86_64")]
const SYS_SHUTDOWN: usize = 48;
#[cfg(target_arch = "x86_64")]
const SYS_FCNTL: usize = 72;

#[cfg(target_arch = "aarch64")]
const SYS_CLOSE: usize = 57;
#[cfg(target_arch = "aarch64")]
const SYS_SHUTDOWN: usize = 210;
#[cfg(target_arch = "aarch64")]
const SYS_SENDMSG: usize = 211;
#[cfg(target_arch = "aarch64")]
const SYS_RECVMSG: usize = 212;
#[cfg(target_arch = "aarch64")]
const SYS_FCNTL: usize = 25;
