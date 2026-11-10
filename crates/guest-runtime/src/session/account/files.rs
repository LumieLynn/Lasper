use core::ffi::CStr;

use crate::linux::uapi::{
    errno::{EACCES, EFBIG, EINVAL},
    file::{AT_FDCWD, O_CLOEXEC, O_NOCTTY, O_NONBLOCK, O_PATH, S_IFMT, S_IFREG, X_OK},
    nr,
};
use crate::linux::{result_or_errno, Descriptor};
use crate::syscall::{syscall1, syscall3};

use super::MAX_ACCOUNT_FILE_BYTES;

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
    result_or_errno(unsafe { syscall3(nr::FACCESSAT, AT_FDCWD, path.as_ptr() as usize, X_OK) })?;
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
    let stat = crate::linux::filesystem::stat_fd(&file)?;
    if stat.mode & S_IFMT != S_IFREG {
        return Err(EACCES);
    }
    Ok(file)
}
