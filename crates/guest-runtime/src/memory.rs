use core::ptr::NonNull;

use crate::linux::uapi::{
    errno::{EINVAL, ENOMEM},
    memory::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE},
    nr,
};
use crate::syscall::{syscall2, syscall6};

/// Owns zero-filled anonymous pages. Borrowed storage cannot outlive the mapping.
pub(crate) struct Mapping {
    pointer: NonNull<u8>,
    length: usize,
}

impl Mapping {
    pub(crate) fn new(length: usize) -> Result<Self, i32> {
        if length == 0 || length > isize::MAX as usize {
            return Err(EINVAL);
        }
        // SAFETY: No existing mapping is replaced; the kernel chooses the address.
        let result = unsafe {
            syscall6(
                nr::MMAP,
                0,
                length,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                usize::MAX,
                0,
            )
        };
        if result < 0 {
            return Err((-result) as i32);
        }
        let Some(pointer) = NonNull::new(result as *mut u8) else {
            // Rust references cannot represent an otherwise valid mapping at zero.
            unsafe { syscall2(nr::MUNMAP, 0, length) };
            return Err(ENOMEM);
        };
        Ok(Self { pointer, length })
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.pointer.as_ptr()
    }
}

impl AsRef<[u8]> for Mapping {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: mmap initialized this entire live mapping with zero bytes.
        unsafe { core::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}

impl AsMut<[u8]> for Mapping {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: This owner lends exclusive access to its live mapping.
        unsafe { core::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: The owner cannot be dropped while its storage is borrowed.
        unsafe { syscall2(nr::MUNMAP, self.pointer.as_ptr() as usize, self.length) };
    }
}
