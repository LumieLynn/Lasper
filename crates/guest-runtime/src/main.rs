#![no_main]
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

mod cli;
mod entry_memory;
mod entry_relocation;
mod entry_stack;
#[allow(dead_code)]
mod syscall;
#[allow(dead_code)]
#[path = "linux/uapi.rs"]
mod uapi;
use uapi::{errno::EINTR, nr};

use core::arch::{asm, global_asm};
use core::ffi::CStr;
use core::panic::PanicInfo;
use lasper_guest_runtime::failure::FailureClass;
use lasper_guest_runtime::paths;

#[cfg(not(target_os = "linux"))]
compile_error!("the Lasper guest runtime is Linux-only");

#[cfg(panic = "unwind")]
compile_error!("build the Lasper guest runtime with the guest-runtime profile");

// The initial stack contains argc, argv, envp and the auxiliary vector.
#[cfg(target_arch = "x86_64")]
global_asm!(
    ".global _start",
    ".type _start,@function",
    "_start:",
    "mov r12, rsp",
    "and rsp, -16",
    "lea rdi, [rip + __ehdr_start]",
    "lea rsi, [rip + _DYNAMIC]",
    "mov rdx, r12",
    "call {relocate}",
    "mov rdi, r12",
    "call {entry}",
    "ud2",
    entry = sym runtime_entry,
    relocate = sym entry_relocation::relocate,
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    ".global _start",
    ".type _start,%function",
    "_start:",
    "mov x19, sp",
    "adrp x0, __ehdr_start",
    "add x0, x0, :lo12:__ehdr_start",
    "adrp x1, _DYNAMIC",
    "add x1, x1, :lo12:_DYNAMIC",
    "mov x2, x19",
    "bl {relocate}",
    "mov x0, x19",
    "bl {entry}",
    "brk #0",
    entry = sym runtime_entry,
    relocate = sym entry_relocation::relocate,
);

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    exit_group(FailureClass::Internal.exit_code())
}

/// Dispatches the invocation supplied on the Linux process entry stack.
///
/// # Safety
///
/// `initial_stack` must retain the complete kernel-provided layout, including
/// readable argument strings and the terminated auxiliary vector. Its argv slots
/// must be writable. Relocations and RELRO protection must already be complete.
/// Call only from the single-threaded ELF entry before any child is created.
#[no_mangle]
unsafe extern "C" fn runtime_entry(initial_stack: *const usize) -> ! {
    let argc = unsafe { *initial_stack };
    let argv = unsafe { initial_stack.add(1) as *mut *const core::ffi::c_char };
    let envp = unsafe { argv.add(argc + 1) as *const *const core::ffi::c_char };
    let page_size = unsafe { entry_relocation::page_size(initial_stack) };
    // Init entry detection precedes flag parsing so arguments such as --help
    // reach the guest init.
    if argc > 0
        && paths::INIT_CANDIDATES
            .iter()
            .any(|path| unsafe { c_string_equals((*argv).cast(), path.to_bytes()) })
    {
        if unsafe { syscall::syscall1(nr::GETPID, 0) } != 1 {
            let _ = write_all(
                2,
                b"lasper guest runtime: automatic init wrapper requires PID 1\n",
            );
            exit_group(FailureClass::Usage.exit_code());
        }
        let init = paths::ORIGINAL_INIT.as_ptr();
        // BusyBox chooses its init applet from argv[0].
        unsafe { *argv = init };
        let invocation = entry_stack::Invocation::Wrapper {
            executable: init,
            argv,
            envp,
        };
        unsafe { entry_stack::run(&invocation, page_size) };
    }
    if argc >= 2 && unsafe { c_string_equals((*argv.add(1)).cast(), cli::WRAP_COMMAND) } {
        if argc == 3 && unsafe { is_help((*argv.add(2)).cast()) } {
            exit_group(if write_all(1, cli::WRAP_HELP) {
                0
            } else {
                FailureClass::Io.exit_code()
            });
        }
        if argc >= 4 && unsafe { c_string_equals((*argv.add(2)).cast(), b"--") } {
            let payload = unsafe { argv.add(3) };
            let invocation = entry_stack::Invocation::Wrapper {
                executable: unsafe { *payload },
                argv: payload,
                envp,
            };
            unsafe { entry_stack::run(&invocation, page_size) };
        }
        let _ = write_all(2, cli::WRAP_ERROR);
        exit_group(FailureClass::Usage.exit_code());
    }
    if argc == 3 {
        let argument = unsafe { *initial_stack.add(2) as *const u8 };
        if unsafe { c_string_equals(argument, cli::BROKER_ARGUMENT) } {
            let directory =
                unsafe { CStr::from_ptr(*initial_stack.add(3) as *const core::ffi::c_char) };
            unsafe {
                entry_stack::run(
                    &entry_stack::Invocation::Broker(directory.as_ptr()),
                    page_size,
                )
            };
        }
    }
    if argc == 2 {
        let argument = unsafe { *initial_stack.add(2) as *const u8 };
        if unsafe { c_string_equals(argument, cli::IDENTIFY_ARGUMENT) } {
            let identity = lasper_guest_runtime::identity_frame();
            if write_all(1, &identity) {
                exit_group(0);
            }
            exit_group(FailureClass::Io.exit_code());
        }
        if unsafe { is_help(argument) } {
            if write_all(1, cli::HELP) {
                exit_group(0);
            }
            exit_group(FailureClass::Io.exit_code());
        }
    }

    let _ = write_all(2, cli::ERROR);
    exit_group(FailureClass::Usage.exit_code())
}

/// # Safety
///
/// `value` must point to a readable NUL-terminated string within one allocation.
unsafe fn is_help(value: *const u8) -> bool {
    unsafe {
        c_string_equals(value, cli::HELP_ARGUMENT)
            || c_string_equals(value, cli::HELP_SHORT_ARGUMENT)
    }
}

/// # Safety
///
/// `value` must point to a readable NUL-terminated string within one allocation.
unsafe fn c_string_equals(value: *const u8, expected: &[u8]) -> bool {
    let mut index = 0;
    while index < expected.len() {
        let byte = unsafe { *value.add(index) };
        if byte == 0 || byte != expected[index] {
            return false;
        }
        index += 1;
    }
    unsafe { *value.add(expected.len()) == 0 }
}

fn write_all(fd: usize, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let written = write(fd, bytes);
        if written == -(EINTR as isize) {
            continue;
        }
        if written <= 0 {
            return false;
        }
        bytes = &bytes[written as usize..];
    }
    true
}

#[cfg(target_arch = "x86_64")]
fn write(fd: usize, bytes: &[u8]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr::WRITE => result,
            in("rdi") fd,
            in("rsi") bytes.as_ptr(),
            in("rdx") bytes.len(),
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
fn write(fd: usize, bytes: &[u8]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") fd as isize => result,
            in("x1") bytes.as_ptr(),
            in("x2") bytes.len(),
            in("x8") nr::WRITE,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
fn exit_group(code: i32) -> ! {
    unsafe {
        asm!(
            "syscall",
            in("rax") nr::EXIT_GROUP,
            in("rdi") code as usize,
            options(noreturn),
        );
    }
}

#[cfg(target_arch = "aarch64")]
fn exit_group(code: i32) -> ! {
    unsafe {
        asm!(
            "svc 0",
            in("x0") code as usize,
            in("x8") nr::EXIT_GROUP,
            options(noreturn),
        );
    }
}
