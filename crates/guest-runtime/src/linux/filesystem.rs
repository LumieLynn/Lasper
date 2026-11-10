use core::ffi::CStr;
use core::mem::{align_of, offset_of, size_of};

use super::{result_or_errno, Descriptor};
use crate::linux::uapi::{
    errno::EIO,
    file::{AT_EMPTY_PATH, AT_FDCWD, STATX_INO, STATX_MODE, STATX_NLINK, STATX_TYPE, STATX_UID},
    nr,
};
use crate::syscall::syscall5;

pub(crate) fn stat_fd(file: &Descriptor) -> Result<Statx, i32> {
    stat(file.raw() as usize, c"", AT_EMPTY_PATH, STATX_TYPE)
}

pub(crate) fn ownership(file: &Descriptor) -> Result<Statx, i32> {
    stat(
        file.raw() as usize,
        c"",
        AT_EMPTY_PATH,
        STATX_TYPE | STATX_MODE | STATX_UID | STATX_NLINK | STATX_INO,
    )
}

pub(crate) fn stat_path(path: &CStr) -> Result<Statx, i32> {
    stat(AT_FDCWD, path, 0, STATX_TYPE | STATX_INO)
}

pub(crate) fn stat_at(directory: &Descriptor, path: &CStr) -> Result<Statx, i32> {
    stat(
        directory.raw() as usize,
        path,
        crate::linux::uapi::file::AT_SYMLINK_NOFOLLOW,
        STATX_TYPE | STATX_INO,
    )
}

fn stat(fd: usize, path: &CStr, flags: usize, mask: usize) -> Result<Statx, i32> {
    let mut stat = Statx::default();
    // SAFETY: Statx matches the Linux UAPI on both supported architectures.
    // The live C string and exclusive output buffer cover the syscall.
    result_or_errno(unsafe {
        syscall5(
            nr::STATX,
            fd,
            path.as_ptr() as usize,
            flags,
            mask,
            &mut stat as *mut _ as usize,
        )
    })?;
    if stat.mask & mask as u32 != mask as u32 {
        return Err(EIO);
    }
    Ok(stat)
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct Statx {
    mask: u32,
    block_size: u32,
    attributes: u64,
    pub(crate) links: u32,
    pub(crate) uid: u32,
    gid: u32,
    pub(crate) mode: u16,
    reserved: u16,
    pub(crate) inode: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    timestamps: [u64; 8],
    rdev_major: u32,
    rdev_minor: u32,
    device_major: u32,
    device_minor: u32,
    remainder: [u64; 14],
}

impl Statx {
    pub(crate) const fn device(&self) -> u64 {
        let major = self.device_major as u64;
        let minor = self.device_minor as u64;
        ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
    }
}

const _: () = {
    assert!(size_of::<Statx>() == 256);
    assert!(align_of::<Statx>() == 8);
    assert!(offset_of!(Statx, mode) == 28);
    assert!(offset_of!(Statx, inode) == 32);
    assert!(offset_of!(Statx, device_major) == 136);
};
