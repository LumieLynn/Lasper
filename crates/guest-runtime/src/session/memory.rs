use core::mem::{align_of, size_of, MaybeUninit};
use lasper_guest_protocol::{MAX_COMMAND_TOTAL_BYTES, MAX_IDENTITY_MESSAGE_BYTES};

use crate::memory::Mapping;

use super::account::{Account, MAX_ACCOUNT_FILE_BYTES};

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

pub(super) struct WorkerStorage {
    pub(super) account: MaybeUninit<Account>,
    pub(super) scratch: [u8; SCRATCH_BYTES],
    pub(super) packet: [u8; MAX_IDENTITY_MESSAGE_BYTES],
}

const _: () = assert!(size_of::<WorkerStorage>() <= WORKER_MEMORY_BYTES);
const _: () = assert!(align_of::<WorkerStorage>() <= 4096);

pub(super) struct WorkerMemory(Mapping);

impl WorkerMemory {
    pub(super) fn new() -> Result<Self, i32> {
        Mapping::new(WORKER_MEMORY_BYTES).map(Self)
    }

    pub(super) fn storage(&mut self) -> &mut WorkerStorage {
        // SAFETY: mmap provides aligned, zero-filled storage of the checked
        // size. Its fields are byte arrays and a still-uninitialized Account.
        // This owner keeps the mapping alive and only lends exclusive access.
        unsafe { &mut *self.0.as_mut_ptr().cast::<WorkerStorage>() }
    }
}
