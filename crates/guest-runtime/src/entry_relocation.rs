use super::syscall::syscall3;
use super::uapi::{memory::PROT_READ, nr};

#[repr(C)]
pub(super) struct Dynamic {
    tag: usize,
    value: usize,
}
#[repr(C)]
struct Rela {
    offset: usize,
    info: usize,
    addend: isize,
}

/// Relocates the static PIE and makes RELRO pages read-only.
///
/// No dynamic loader runs for this ELF, so relocation must finish before Rust
/// uses function pointers or static references.
///
/// # Safety
///
/// `base` and `dynamic` must describe this process's trusted, mapped ELF image.
/// All referenced metadata must be readable and relocation targets writable.
/// `stack` must satisfy [`page_size`]'s requirements. Call once, before code
/// uses relocated pointers or protected data.
pub(super) unsafe extern "C" fn relocate(
    base: usize,
    dynamic: *const Dynamic,
    stack: *const usize,
) {
    let page_size = unsafe { page_size(stack) };
    let mut item = dynamic;
    let mut table = 0usize;
    let mut bytes = 0usize;
    let mut entry_size = 0usize;
    loop {
        let tag = unsafe { (*item).tag };
        let value = unsafe { (*item).value };
        match tag {
            0 => break,
            7 => table = base.wrapping_add(value),
            8 => bytes = value,
            9 => entry_size = value,
            _ => {}
        }
        item = unsafe { item.add(1) };
    }
    if bytes == 0 {
        unsafe { protect_relro(base, page_size) };
        return;
    }
    if entry_size != core::mem::size_of::<Rela>() || !bytes.is_multiple_of(entry_size) {
        super::exit_group(127);
    }
    let mut offset = 0;
    while offset < bytes {
        let relocation = table.wrapping_add(offset) as *const Rela;
        #[cfg(target_arch = "x86_64")]
        const RELATIVE: usize = 8;
        #[cfg(target_arch = "aarch64")]
        const RELATIVE: usize = 1027;
        if unsafe { (*relocation).info } != RELATIVE {
            super::exit_group(127);
        }
        let destination = base.wrapping_add(unsafe { (*relocation).offset }) as *mut usize;
        unsafe { destination.write(base.wrapping_add_signed((*relocation).addend)) };
        offset += entry_size;
    }
    unsafe { protect_relro(base, page_size) };
}

#[repr(C)]
struct ElfHeader {
    prefix: [u8; 32],
    program_offset: u64,
    section_offset: u64,
    flags: u32,
    header_bytes: u16,
    program_bytes: u16,
    program_count: u16,
    rest: [u8; 6],
}

#[repr(C)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    address: u64,
    physical_address: u64,
    file_bytes: u64,
    memory_bytes: u64,
    alignment: u64,
}

/// Applies RELRO protection that the kernel leaves to the ELF interpreter.
///
/// # Safety
///
/// `base` must describe the same trusted image as [`relocate`], with all relative
/// relocations complete. `page_size` must be the kernel page size.
unsafe fn protect_relro(base: usize, page_size: usize) {
    let header = base as *const ElfHeader;
    if unsafe { (*header).program_bytes as usize } != core::mem::size_of::<ProgramHeader>() {
        super::exit_group(127);
    }
    let table =
        base.wrapping_add(unsafe { (*header).program_offset as usize }) as *const ProgramHeader;
    for index in 0..unsafe { (*header).program_count as usize } {
        let program = unsafe { &*table.add(index) };
        if program.kind == 0x6474_e552 && program.memory_bytes != 0 {
            let address = match base.checked_add(program.address as usize) {
                Some(address) => address,
                None => super::exit_group(127),
            };
            let Some(end) = address.checked_add(program.memory_bytes as usize) else {
                super::exit_group(127);
            };
            // Rounding an unaligned end down would leave RELRO bytes writable.
            // This check is needed even when artifact verification was skipped.
            if !end.is_multiple_of(page_size) {
                super::exit_group(127);
            }
            let start = address & !(page_size - 1);
            let result = unsafe { syscall3(nr::MPROTECT, start, end - start, PROT_READ) };
            if result < 0 {
                super::exit_group(127);
            }
        }
    }
}

/// Reads `AT_PAGESZ` from the initial auxiliary vector.
///
/// # Safety
///
/// `stack` must point to a complete, readable Linux initial stack. The argc value,
/// pointer-array terminators and auxiliary-vector terminator must match that layout.
pub(super) unsafe fn page_size(stack: *const usize) -> usize {
    let mut cursor = unsafe { stack.add(*stack + 2) };
    while unsafe { *cursor } != 0 {
        cursor = unsafe { cursor.add(1) };
    }
    cursor = unsafe { cursor.add(1) };
    while unsafe { *cursor } != 0 {
        if unsafe { *cursor } == 6 {
            let size = unsafe { *cursor.add(1) };
            if size.is_power_of_two() {
                return size;
            }
            break;
        }
        cursor = unsafe { cursor.add(2) };
    }
    super::exit_group(127)
}

const _: () = {
    assert!(core::mem::size_of::<Dynamic>() == 16);
    assert!(core::mem::align_of::<Dynamic>() == 8);
    assert!(core::mem::size_of::<Rela>() == 24);
    assert!(core::mem::offset_of!(Rela, info) == 8);
    assert!(core::mem::offset_of!(Rela, addend) == 16);
    assert!(core::mem::size_of::<ElfHeader>() == 64);
    assert!(core::mem::offset_of!(ElfHeader, program_bytes) == 54);
    assert!(core::mem::size_of::<ProgramHeader>() == 56);
    assert!(core::mem::offset_of!(ProgramHeader, address) == 16);
    assert!(core::mem::offset_of!(ProgramHeader, memory_bytes) == 40);
};
