use core::ffi::CStr;
use core::fmt::{self, Write};

use super::uapi::errno::{EIO, ESRCH};
use super::{self as linux, Descriptor};

pub(crate) fn visit_direct_children(
    parent: i32,
    tid: i32,
    mut visit: impl FnMut(i32) -> Result<(), i32>,
) -> Result<(), i32> {
    let mut path = ProcPath::new();
    write!(path, "/proc/self/task/{tid}/children").map_err(|_| EIO)?;
    match linux::open(path.as_c_str(), false) {
        Ok(file) => read_children(&file, &mut visit),
        Err(2 | 20) => scan_children(parent, visit),
        Err(errno) => Err(errno),
    }
}

fn read_children(
    file: &Descriptor,
    visit: &mut impl FnMut(i32) -> Result<(), i32>,
) -> Result<(), i32> {
    let mut chunk = [0; 256];
    let mut number = 0i32;
    let mut digits = false;
    loop {
        let length = file.read(&mut chunk)?;
        if length == 0 {
            if digits {
                visit(number)?;
            }
            return Ok(());
        }
        for byte in &chunk[..length] {
            if byte.is_ascii_digit() {
                number = number
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(i32::from(byte - b'0')))
                    .ok_or(EIO)?;
                digits = true;
            } else if byte.is_ascii_whitespace() {
                if digits {
                    visit(number)?;
                }
                number = 0;
                digits = false;
            } else {
                return Err(EIO);
            }
        }
    }
}

fn scan_children(parent: i32, mut visit: impl FnMut(i32) -> Result<(), i32>) -> Result<(), i32> {
    let directory = linux::open(c"/proc", true)?;
    let mut entries = [0; 4096];
    loop {
        let length = linux::read_directory(&directory, &mut entries)?;
        if length == 0 {
            return Ok(());
        }
        linux::for_directory_entries(&entries[..length], |name| {
            let Some(pid) = linux::parse_pid(name).filter(|pid| *pid > 0) else {
                return Ok(());
            };
            let mut path = ProcPath::new();
            write!(path, "/proc/{pid}/stat").map_err(|_| EIO)?;
            let file = match linux::open(path.as_c_str(), false) {
                Ok(file) => file,
                Err(2 | ESRCH) => return Ok(()),
                Err(errno) => return Err(errno),
            };
            let mut stat = [0; 4096];
            let length = file.read(&mut stat)?;
            if stat_parent(&stat[..length]) == Some(parent) {
                visit(pid)?;
            }
            Ok(())
        })?;
    }
}

fn stat_parent(bytes: &[u8]) -> Option<i32> {
    let end = bytes.iter().rposition(|byte| *byte == b')')?;
    let mut fields = bytes[end + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty());
    fields.next()?;
    linux::parse_pid(fields.next()?)
}

struct ProcPath {
    bytes: [u8; 96],
    length: usize,
}

impl ProcPath {
    const fn new() -> Self {
        Self {
            bytes: [0; 96],
            length: 0,
        }
    }

    fn as_c_str(&self) -> &CStr {
        // SAFETY: Only Write appends bytes, leaving a zero byte after length.
        unsafe { CStr::from_bytes_with_nul_unchecked(&self.bytes[..self.length + 1]) }
    }
}

impl Write for ProcPath {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length.checked_add(text.len()).ok_or(fmt::Error)?;
        if end >= self.bytes.len() || text.as_bytes().contains(&0) {
            return Err(fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/linux/children.rs"]
mod tests;
