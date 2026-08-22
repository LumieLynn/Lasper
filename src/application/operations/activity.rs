//! Tracks state-changing host operations that outlive a UI key handler.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone, Default)]
pub struct HostOperationTracker {
    active: Arc<AtomicUsize>,
}

impl HostOperationTracker {
    #[must_use = "the guard must be kept alive for the duration of the host operation"]
    pub fn begin(&self) -> HostOperationGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        HostOperationGuard {
            active: self.active.clone(),
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for HostOperationTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostOperationTracker")
            .field("active", &self.active_count())
            .finish()
    }
}

pub struct HostOperationGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for HostOperationGuard {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "host operation tracker underflow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_track_shared_operation_lifetimes() {
        let tracker = HostOperationTracker::default();
        let first = tracker.begin();
        let second_tracker = tracker.clone();
        let second = second_tracker.begin();

        assert_eq!(tracker.active_count(), 2);
        drop(first);
        assert_eq!(second_tracker.active_count(), 1);
        drop(second);
        assert_eq!(tracker.active_count(), 0);
    }
}
