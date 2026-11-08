use core::arch::asm;

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall1(number: usize, first: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall2(number: usize, first: usize, second: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall3(number: usize, first: usize, second: usize, third: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall4(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            in("r10") fourth,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall5(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
    fifth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            in("r10") fourth,
            in("r8") fifth,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn syscall6(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
    fifth: usize,
    sixth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            in("r10") fourth,
            in("r8") fifth,
            in("r9") sixth,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall1(number: usize, first: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall2(number: usize, first: usize, second: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall3(number: usize, first: usize, second: usize, third: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x2") third,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall4(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x2") third,
            in("x3") fourth,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall5(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
    fifth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x2") third,
            in("x3") fourth,
            in("x4") fifth,
            in("x8") number,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall6(
    number: usize,
    first: usize,
    second: usize,
    third: usize,
    fourth: usize,
    fifth: usize,
    sixth: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc 0",
            inlateout("x0") first as isize => result,
            in("x1") second,
            in("x2") third,
            in("x3") fourth,
            in("x4") fifth,
            in("x5") sixth,
            in("x8") number,
        );
    }
    result
}
