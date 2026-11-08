use core::ffi::CStr;

use crate::session::linux::{result_or_errno, Descriptor, EIO};
use crate::syscall::{syscall1, syscall3, syscall5};

use super::MAX_ACCOUNT_FILE_BYTES;

const AT_FDCWD: usize = -100isize as usize;
const O_CLOEXEC: usize = 0o2000000;
const O_NONBLOCK: usize = 0o4000;
const O_NOCTTY: usize = 0o400;
const O_PATH: usize = 0o10000000;
const AT_EMPTY_PATH: usize = 0x1000;
const STATX_TYPE: usize = 1;
const S_IFMT: u16 = 0o170000;
const S_IFREG: u16 = 0o100000;
const EFBIG: i32 = 27;
const EINVAL: i32 = 22;
const EACCES: i32 = 13;

pub(super) fn read(path: &CStr, buffer: &mut [u8]) -> Result<usize, i32> {
    if buffer.len() <= MAX_ACCOUNT_FILE_BYTES {
        return Err(EINVAL);
    }
    let file = open_regular(path, O_NONBLOCK | O_NOCTTY)?;
    let mut length = 0;
    while length <= MAX_ACCOUNT_FILE_BYTES {
        let count = file.read(&mut buffer[length..=MAX_ACCOUNT_FILE_BYTES])?;
        if count == 0 {
            return Ok(length);
        }
        length += count;
    }
    Err(EFBIG)
}

pub(super) fn enter_home(path: &CStr) -> Result<(), i32> {
    result_or_errno(unsafe { syscall1(nr::CHDIR, path.as_ptr() as usize) })?;
    Ok(())
}

pub(super) fn check_executable(path: &CStr) -> Result<(), i32> {
    let _file = open_regular(path, O_PATH)?;
    // access uses real IDs, which have already been made equal to effective
    // and saved IDs. O_PATH above does not require read permission on a shell.
    result_or_errno(unsafe { syscall3(nr::FACCESSAT, AT_FDCWD, path.as_ptr() as usize, 1) })?;
    Ok(())
}

fn open_regular(path: &CStr, flags: usize) -> Result<Descriptor, i32> {
    let file = Descriptor::adopt(unsafe {
        crate::syscall::syscall4(
            nr::OPENAT,
            AT_FDCWD,
            path.as_ptr() as usize,
            flags | O_CLOEXEC,
            0,
        )
    })?;
    let mut stat = Statx::default();
    // SAFETY: The 256-byte statx buffer has the kernel ABI layout on both
    // supported architectures; AT_EMPTY_PATH observes the already-open file.
    result_or_errno(unsafe {
        syscall5(
            nr::STATX,
            file.raw() as usize,
            c"".as_ptr() as usize,
            AT_EMPTY_PATH,
            STATX_TYPE,
            &mut stat as *mut _ as usize,
        )
    })?;
    if stat.mask & STATX_TYPE as u32 == 0 {
        return Err(EIO);
    }
    if stat.mode & S_IFMT != S_IFREG {
        return Err(EACCES);
    }
    Ok(file)
}

#[repr(C)]
#[derive(Default)]
struct Statx {
    mask: u32,
    block_size: u32,
    attributes: u64,
    links: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    reserved: u16,
    remainder: [u64; 28],
}

const _: () = assert!(core::mem::size_of::<Statx>() == 256);

#[cfg(target_arch = "x86_64")]
mod nr {
    pub(super) const OPENAT: usize = 257;
    pub(super) const FACCESSAT: usize = 269;
    pub(super) const CHDIR: usize = 80;
    pub(super) const STATX: usize = 332;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub(super) const OPENAT: usize = 56;
    pub(super) const FACCESSAT: usize = 48;
    pub(super) const CHDIR: usize = 49;
    pub(super) const STATX: usize = 291;
}
