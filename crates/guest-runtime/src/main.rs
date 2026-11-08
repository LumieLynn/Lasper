#![no_main]
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

// The ELF entry uses only write/exit from the library's shared kernel table.
#[allow(dead_code)]
#[path = "linux/uapi.rs"]
mod uapi;
use uapi::{errno::EINTR, nr};

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[cfg(not(target_os = "linux"))]
compile_error!("the Lasper guest runtime is Linux-only");

#[cfg(panic = "unwind")]
compile_error!("build the Lasper guest runtime with the guest-runtime profile");

const IDENTIFY_ARGUMENT: &[u8] = b"--identify";
const HELP_ARGUMENT: &[u8] = b"--help";
const USAGE: &[u8] = b"Usage: lasper-guest-runtime --identify\n";

// Linux ELF entry ABI: sp points to argc followed by the argv pointers.
#[cfg(target_arch = "x86_64")]
global_asm!(
    ".global _start",
    ".type _start,@function",
    "_start:",
    "mov rdi, rsp",
    "and rsp, -16",
    "call {entry}",
    "ud2",
    entry = sym runtime_entry,
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    ".global _start",
    ".type _start,%function",
    "_start:",
    "mov x0, sp",
    "bl {entry}",
    "brk #0",
    entry = sym runtime_entry,
);

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    exit_group(127)
}

#[no_mangle]
unsafe extern "C" fn runtime_entry(initial_stack: *const usize) -> ! {
    let argc = unsafe { *initial_stack };
    if argc == 2 {
        let argument = unsafe { *initial_stack.add(2) as *const u8 };
        if unsafe { c_string_equals(argument, IDENTIFY_ARGUMENT) } {
            let identity = lasper_guest_runtime::identity_frame();
            if write_all(1, &identity) {
                exit_group(0);
            }
            exit_group(74);
        }
        if unsafe { c_string_equals(argument, HELP_ARGUMENT) } {
            if write_all(1, USAGE) {
                exit_group(0);
            }
            exit_group(74);
        }
    }

    let _ = write_all(2, USAGE);
    exit_group(64)
}

/// # Safety
///
/// `value` must point to a readable, NUL-terminated process argument.
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
