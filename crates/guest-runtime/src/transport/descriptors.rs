use super::TransportError;
use crate::socket::{close, duplicate_cloexec};
use lasper_guest_protocol::MAX_CONTROL_DESCRIPTORS;

// Keep one overflow slot so an excessive FD count can be rejected explicitly.
pub(super) const MAX_RECEIVED_DESCRIPTORS: usize = MAX_CONTROL_DESCRIPTORS as usize + 1;
const INVALID_FD: i32 = -1;

/// Owns descriptors installed by `SCM_RIGHTS`. Dropping it closes them.
pub struct ReceivedDescriptors {
    raw: [i32; MAX_CONTROL_DESCRIPTORS as usize],
    count: usize,
}

impl ReceivedDescriptors {
    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn as_raw_fds(&self) -> &[i32] {
        &self.raw[..self.count]
    }

    /// Transfers ownership to the caller. Only the first returned `count`
    /// entries are valid and must eventually be closed.
    pub fn into_raw_fds(mut self) -> ([i32; MAX_CONTROL_DESCRIPTORS as usize], usize) {
        let raw = self.raw;
        let count = self.count;
        self.count = 0;
        self.raw = [INVALID_FD; MAX_CONTROL_DESCRIPTORS as usize];
        (raw, count)
    }
}

impl Drop for ReceivedDescriptors {
    fn drop(&mut self) {
        for fd in &self.raw[..self.count] {
            let _ = close(*fd);
        }
    }
}

pub(super) struct RawDescriptors {
    raw: [i32; MAX_RECEIVED_DESCRIPTORS],
    pub(super) count: usize,
}

impl RawDescriptors {
    pub(super) const fn new() -> Self {
        Self {
            raw: [INVALID_FD; MAX_RECEIVED_DESCRIPTORS],
            count: 0,
        }
    }

    pub(super) fn push(&mut self, fd: i32) {
        if self.count < self.raw.len() {
            self.raw[self.count] = fd;
            self.count += 1;
        } else {
            let _ = close(fd);
        }
    }

    pub(super) fn finish(mut self, expected: u16) -> Result<ReceivedDescriptors, TransportError> {
        if self.count != expected as usize {
            return Err(TransportError::DescriptorCountMismatch {
                declared: expected,
                actual: self.count,
            });
        }
        let mut result = ReceivedDescriptors {
            raw: [INVALID_FD; MAX_CONTROL_DESCRIPTORS as usize],
            count: self.count,
        };
        for index in 0..self.count {
            result.raw[index] = self.raw[index];
            self.raw[index] = INVALID_FD;
        }
        self.count = 0;
        Ok(result)
    }
}

impl Drop for RawDescriptors {
    fn drop(&mut self) {
        for fd in &self.raw[..self.count] {
            let _ = close(*fd);
        }
    }
}

pub(super) struct SendingDescriptors(RawDescriptors);

impl SendingDescriptors {
    pub(super) fn duplicate(descriptors: &[i32]) -> Result<Self, TransportError> {
        let mut owned = RawDescriptors::new();
        for &fd in descriptors {
            owned.push(duplicate_cloexec(fd).map_err(TransportError::Syscall)?);
        }
        Ok(Self(owned))
    }

    pub(super) fn as_raw_fds(&self) -> &[i32] {
        &self.0.raw[..self.0.count]
    }
}
