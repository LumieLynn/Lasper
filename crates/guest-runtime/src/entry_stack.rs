use core::arch::global_asm;
use core::ffi::{c_char, CStr};
use core::fmt::Write;
use lasper_guest_runtime::diagnostic::Diagnostic;
use lasper_guest_runtime::failure::FailureClass;

use super::syscall::{syscall2, syscall3, syscall6};
use super::uapi::{
    memory::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE},
    nr,
};

// Forked workers inherit live broker and guardian frames in addition to their
// own setup frames.
const BROKER_STACK_BYTES: usize =
    lasper_guest_runtime::session::WORKER_STACK_BUDGET_BYTES + 64 * 1024;

unsafe extern "C" {
    /// # Safety
    ///
    /// `top` must be 16-byte aligned and end an exclusively owned, writable stack
    /// with at least `BROKER_STACK_BYTES` usable bytes. `invocation` must point to
    /// a live value, and the caller must satisfy [`run`]'s requirements.
    fn enter_broker_stack(top: *mut u8, invocation: *const Invocation) -> !;
}

/// Carries borrowed launch arguments across the stack switch.
///
/// # Safety
///
/// Before passing this value to [`run`], every string pointer must refer to a
/// readable NUL-terminated string. Each string, including its terminator, must
/// fit within one allocation and contain at most `isize::MAX` bytes.
///
/// For `Wrapper`, `argv` and `envp` must point to readable, aligned arrays within
/// single allocations, each ending in a null pointer. Every element before that
/// terminator must point to a valid string as above. `argv[0]` must be present.
///
/// The value, arrays and strings must stay valid and unmodified until `execve`
/// or process exit. They may live on the original stack, which [`run`] keeps mapped.
#[repr(C)]
pub(super) enum Invocation {
    Broker(*const c_char),
    Wrapper {
        executable: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    },
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

/// Runs an invocation on a private stack without returning to the original stack.
///
/// The original stack stays mapped for the borrowed launch arguments. Its caller
/// frames are never resumed or dropped.
///
/// # Safety
///
/// `invocation` must satisfy [`Invocation`]'s pointer and lifetime requirements.
/// The process must be single-threaded, have no children and exclusively own
/// child creation and reaping. `page_size` must be the kernel page size obtained
/// from `AT_PAGESZ`.
pub(super) unsafe fn run(invocation: &Invocation, page_size: usize) -> ! {
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
        fail(
            FailureClass::System,
            "allocate broker stack",
            -mapping as i32,
        );
    }
    let stack = mapping as usize + page_size;
    // The bottom page remains inaccessible to guard against stack overflow.
    let result = unsafe { syscall3(nr::MPROTECT, stack, usable, PROT_READ | PROT_WRITE) };
    if result < 0 {
        unsafe { syscall2(nr::MUNMAP, mapping as usize, length) };
        fail(
            FailureClass::System,
            "make broker stack writable",
            -result as i32,
        );
    }
    // SAFETY: mmap provides a separate stack with the required space and alignment.
    // The invocation and its borrowed storage remain on their original mappings.
    unsafe { enter_broker_stack((stack + usable) as *mut u8, invocation) }
}

/// # Safety
///
/// `invocation` must point to the value retained by [`run`]. The caller must have
/// switched to the stack allocated there and satisfy its process requirements.
unsafe extern "C" fn broker_entry(invocation: *const Invocation) -> ! {
    let result = match unsafe { &*invocation } {
        Invocation::Broker(directory) => unsafe {
            lasper_guest_runtime::broker::run(CStr::from_ptr(*directory))
        },
        Invocation::Wrapper {
            executable,
            argv,
            envp,
        } => unsafe {
            lasper_guest_runtime::wrapper::run(
                lasper_guest_runtime::paths::RUNTIME_DIRECTORY,
                CStr::from_ptr(*executable),
                *argv,
                *envp,
            )
            .map(|never| match never {})
        },
    };
    match result {
        Ok(()) => super::exit_group(0),
        Err(error) => fail(error.class, error.operation, error.errno),
    }
}

fn fail(class: FailureClass, operation: &str, errno: i32) -> ! {
    let _ = writeln!(
        Diagnostic,
        "lasper guest runtime: {class:?}: {operation} (errno {errno})"
    );
    super::exit_group(class.exit_code())
}
