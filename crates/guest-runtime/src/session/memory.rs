use core::mem::{align_of, size_of, MaybeUninit};
use core::ptr::NonNull;
use lasper_guest_protocol::{MAX_COMMAND_TOTAL_BYTES, MAX_IDENTITY_MESSAGE_BYTES};

use crate::syscall::{syscall2, syscall6};

use super::account::{Account, MAX_ACCOUNT_FILE_BYTES};
use super::linux::result_or_errno;

/// One private mapping per worker, retained through exec/exit. This is a
/// multiple of all supported Linux page sizes (4, 16 and 64 KiB).
pub const WORKER_MEMORY_BYTES: usize = 192 * 1024;

/// Stack budget for the dedicated guardian's call chain through worker exec,
/// including entry/observer code. The large workspace is mapped separately.
pub const WORKER_STACK_BUDGET_BYTES: usize = 1024 * 1024;

// File reads need one extra byte to distinguish an exact-limit file from an
// oversized one. argv counts separators but still needs its final NUL byte.
const SCRATCH_BYTES: usize = if MAX_ACCOUNT_FILE_BYTES > MAX_COMMAND_TOTAL_BYTES {
    MAX_ACCOUNT_FILE_BYTES + 1
} else {
    MAX_COMMAND_TOTAL_BYTES + 1
};
const PROT_READ_WRITE: usize = 1 | 2;
const MAP_PRIVATE_ANONYMOUS: usize = 2 | 0x20;
const ENOMEM: i32 = 12;

pub(super) struct WorkerStorage {
    pub(super) account: MaybeUninit<Account>,
    pub(super) scratch: [u8; SCRATCH_BYTES],
    pub(super) packet: [u8; MAX_IDENTITY_MESSAGE_BYTES],
}

const _: () = assert!(size_of::<WorkerStorage>() <= WORKER_MEMORY_BYTES);
const _: () = assert!(align_of::<WorkerStorage>() <= 4096);

pub(super) struct WorkerMemory(NonNull<WorkerStorage>);

impl WorkerMemory {
    pub(super) fn new() -> Result<Self, i32> {
        let address = result_or_errno(unsafe {
            syscall6(
                nr::MMAP,
                0,
                WORKER_MEMORY_BYTES,
                PROT_READ_WRITE,
                MAP_PRIVATE_ANONYMOUS,
                usize::MAX,
                0,
            )
        })?;
        let Some(pointer) = NonNull::new(address as *mut WorkerStorage) else {
            // A mapping at zero is legal for Linux, but cannot back Rust references.
            unsafe { syscall2(nr::MUNMAP, address, WORKER_MEMORY_BYTES) };
            return Err(ENOMEM);
        };
        Ok(Self(pointer))
    }

    pub(super) fn storage(&mut self) -> &mut WorkerStorage {
        // SAFETY: mmap provides aligned, zero-filled storage of the checked
        // size. Its fields are byte arrays and a still-uninitialized Account.
        // This owner keeps the mapping alive and only lends exclusive access.
        unsafe { self.0.as_mut() }
    }
}

impl Drop for WorkerMemory {
    fn drop(&mut self) {
        // SAFETY: The mapping belongs to this owner; no storage borrow survives Drop.
        unsafe { syscall2(nr::MUNMAP, self.0.as_ptr() as usize, WORKER_MEMORY_BYTES) };
    }
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub(super) const MMAP: usize = 9;
    pub(super) const MUNMAP: usize = 11;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub(super) const MMAP: usize = 222;
    pub(super) const MUNMAP: usize = 215;
}
