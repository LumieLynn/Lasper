//! Diagnostic output for startup and forked runtime processes.

use crate::linux;

/// Writes directly to the current process's stderr without allocation or buffering.
pub struct Diagnostic;

impl core::fmt::Write for Diagnostic {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            match linux::write(2, bytes) {
                Ok(0) | Err(_) => return Err(core::fmt::Error),
                Ok(count) => bytes = &bytes[count..],
            }
        }
        Ok(())
    }
}
