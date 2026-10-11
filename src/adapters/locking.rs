//! Shared lock-wait policy for adapter-owned sidecar and trusted-state locks.

use crate::adapters::error::NspawnError;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
pub(crate) const DEFAULT_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug)]
pub(crate) struct LockWaitPolicy {
    pub(crate) timeout: Duration,
    pub(crate) retry_delay: Duration,
}

impl Default for LockWaitPolicy {
    fn default() -> Self {
        Self {
            // Retain the sidecar writer's established budget and apply the same
            // bound to trusted state instead of waiting forever. Calibrate it
            // only after contention measurements justify a product-level change.
            timeout: DEFAULT_LOCK_TIMEOUT,
            retry_delay: DEFAULT_LOCK_RETRY_DELAY,
        }
    }
}

pub(crate) fn is_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EACCES
                || code == libc::EAGAIN
                || code == libc::EWOULDBLOCK
        )
}

pub(crate) fn timeout_error(lock_path: &Path, started: Instant, attempts: usize) -> NspawnError {
    let waited_ms = started.elapsed().as_millis();
    NspawnError::Io(
        lock_path.to_path_buf(),
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "lock acquisition deadline exceeded after {waited_ms} ms ({attempts} attempts)"
            ),
        ),
    )
}

pub(crate) fn remaining(policy: LockWaitPolicy, started: Instant) -> Option<Duration> {
    policy.timeout.checked_sub(started.elapsed())
}
