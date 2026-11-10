use core::arch::global_asm;
use core::ffi::{c_char, CStr};
use core::fmt::Write;

use super::syscall::{syscall2, syscall3, syscall6};
use super::uapi::{
    memory::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE},
    nr,
};

// The broker's live frames remain below the guardian call chain after fork.
const BROKER_STACK_BYTES: usize =
    lasper_guest_runtime::session::WORKER_STACK_BUDGET_BYTES + 64 * 1024;

unsafe extern "C" {
    fn enter_broker_stack(top: *mut u8, directory: *const c_char) -> !;
}

#[cfg(target_arch = "x86_64")]
global_asm!(
    ".type enter_broker_stack,@function",
    "enter_broker_stack:",
    "mov rsp, rdi",
    "mov rdi, rsi",
    "call {entry}",
    "ud2",
    entry = sym broker_entry,
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    ".type enter_broker_stack,%function",
    "enter_broker_stack:",
    "mov sp, x0",
    "mov x0, x1",
    "bl {entry}",
    "brk #0",
    entry = sym broker_entry,
);

// Called only from the standalone entry. The directory argument remains on the
// original process stack; the new mapping lives until this process exits.
pub(super) unsafe fn run(directory: *const c_char, page_size: usize) -> ! {
    let usable = BROKER_STACK_BYTES.next_multiple_of(page_size);
    let length = usable + page_size;
    let mapping = unsafe {
        syscall6(
            nr::MMAP,
            0,
            length,
            0,
            MAP_PRIVATE | MAP_ANONYMOUS,
            usize::MAX,
            0,
        )
    };
    if mapping < 0 {
        fail("allocate broker stack", -mapping as i32);
    }
    let stack = mapping as usize + page_size;
    // Leave the bottom page inaccessible so exhaustion faults before crossing
    // into another mapping. Kernel page allocation needs no C runtime or heap.
    let result = unsafe { syscall3(nr::MPROTECT, stack, usable, PROT_READ | PROT_WRITE) };
    if result < 0 {
        unsafe { syscall2(nr::MUNMAP, mapping as usize, length) };
        fail("make broker stack writable", -result as i32);
    }
    // SAFETY: The new stack is writable, page-aligned and exclusively owned.
    // No caller frame is resumed or dropped after the assembly switch.
    unsafe { enter_broker_stack((stack + usable) as *mut u8, directory) }
}

unsafe extern "C" fn broker_entry(directory: *const c_char) -> ! {
    let directory = unsafe { CStr::from_ptr(directory) };
    // SAFETY: The ELF entry is single-threaded and owns no children. Its mapped
    // stack includes the worker budget plus space for the broker's live frames.
    match unsafe { lasper_guest_runtime::broker::run(directory) } {
        Ok(()) => super::exit_group(0),
        Err(error) => fail(error.operation, error.errno),
    }
}

fn fail(operation: &str, errno: i32) -> ! {
    let _ = writeln!(
        super::Diagnostic,
        "lasper guest runtime: {operation} (errno {errno})"
    );
    super::exit_group(70)
}
