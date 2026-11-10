use core::ffi::c_void;

// LLVM may emit these calls even though the runtime never links a C library.
// Volatile byte accesses keep their implementations from becoming self-calls.

#[no_mangle]
unsafe extern "C" fn memcpy(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    for offset in 0..count {
        unsafe {
            destination
                .cast::<u8>()
                .add(offset)
                .write_volatile(source.cast::<u8>().add(offset).read_volatile())
        };
    }
    destination
}

#[no_mangle]
unsafe extern "C" fn memmove(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    if destination as usize <= source as usize {
        unsafe { memcpy(destination, source, count) }
    } else {
        for offset in (0..count).rev() {
            unsafe {
                destination
                    .cast::<u8>()
                    .add(offset)
                    .write_volatile(source.cast::<u8>().add(offset).read_volatile())
            };
        }
        destination
    }
}

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

#[no_mangle]
unsafe extern "C" fn bcmp(left: *const c_void, right: *const c_void, count: usize) -> i32 {
    unsafe { memcmp(left, right, count) }
}
