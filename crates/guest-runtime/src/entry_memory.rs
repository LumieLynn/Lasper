//! Supplements `compiler_builtins` with C ABI functions used by the libc-free ELF.
//!
//! `CStr::from_ptr` uses `strlen` when the wrapper checks the payload path. LLVM can
//! emit calls to the memory functions directly. Volatile byte accesses keep
//! these implementations from becoming calls to themselves.
//!
//! Callers must keep all accessed memory valid and prevent conflicting concurrent
//! accesses. The memory functions access nothing when `count` is zero.

use core::ffi::{c_char, c_void};

/// # Safety
///
/// `value` must be readable through a NUL terminator in the same allocation,
/// with the terminator less than `isize::MAX` bytes from `value`.
#[no_mangle]
unsafe extern "C" fn strlen(value: *const c_char) -> usize {
    let mut length = 0;
    while unsafe { value.add(length).read_volatile() } != 0 {
        length += 1;
    }
    length
}

/// # Safety
///
/// `source` must be readable and `destination` writable for `count` bytes.
/// The ranges must not overlap.
#[no_mangle]
unsafe extern "C" fn memcpy(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    unsafe { copy_forward(destination, source, count) };
    destination
}

/// # Safety
///
/// `source` must be readable and `destination` writable for `count` bytes.
/// If the ranges overlap, `destination` must start at or below `source`.
unsafe fn copy_forward(destination: *mut c_void, source: *const c_void, count: usize) {
    for offset in 0..count {
        unsafe {
            destination
                .cast::<u8>()
                .add(offset)
                .write_volatile(source.cast::<u8>().add(offset).read_volatile())
        };
    }
}

/// # Safety
///
/// `source` must be readable and `destination` writable for `count` bytes.
/// The ranges may overlap.
#[no_mangle]
unsafe extern "C" fn memmove(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    if destination as usize <= source as usize {
        unsafe { copy_forward(destination, source, count) };
    } else {
        for offset in (0..count).rev() {
            unsafe {
                destination
                    .cast::<u8>()
                    .add(offset)
                    .write_volatile(source.cast::<u8>().add(offset).read_volatile())
            };
        }
    }
    destination
}

/// # Safety
///
/// `destination` must be writable for `count` bytes.
#[no_mangle]
unsafe extern "C" fn memset(destination: *mut c_void, value: i32, count: usize) -> *mut c_void {
    for offset in 0..count {
        unsafe {
            destination
                .cast::<u8>()
                .add(offset)
                .write_volatile(value as u8)
        };
    }
    destination
}

/// # Safety
///
/// Both pointers must be readable for `count` bytes. The ranges may overlap.
#[no_mangle]
unsafe extern "C" fn memcmp(left: *const c_void, right: *const c_void, count: usize) -> i32 {
    for offset in 0..count {
        let difference = unsafe {
            i32::from(left.cast::<u8>().add(offset).read_volatile())
                - i32::from(right.cast::<u8>().add(offset).read_volatile())
        };
        if difference != 0 {
            return difference;
        }
    }
    0
}

/// # Safety
///
/// Both pointers must be readable for `count` bytes. The ranges may overlap.
#[no_mangle]
unsafe extern "C" fn bcmp(left: *const c_void, right: *const c_void, count: usize) -> i32 {
    unsafe { memcmp(left, right, count) }
}
