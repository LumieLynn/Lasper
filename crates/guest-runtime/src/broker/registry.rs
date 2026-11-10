use crate::linux::uapi::{errno::ESRCH, signal::SIGCONT};
use crate::linux::{self, Descriptor};
use crate::session::GUARDIAN_START_BYTE;

pub(super) const CAPACITY: usize = 32;

pub(super) struct Registry {
    slots: [Option<Guardian>; CAPACITY],
}

struct Guardian {
    pid: i32,
    pidfd: Option<Descriptor>,
    release: Option<Descriptor>,
}

impl Registry {
    pub(super) const fn new() -> Self {
        Self {
            slots: [const { None }; CAPACITY],
        }
    }

    pub(super) fn reserve(&mut self) -> Option<Reservation<'_>> {
        self.slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .map(|slot| Reservation { slot })
    }

    pub(super) fn contains(&self, pid: i32) -> bool {
        self.slots.iter().flatten().any(|child| child.pid == pid)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    pub(super) fn reaped(&mut self, pid: i32) -> bool {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|child| child.pid == pid))
        else {
            return false;
        };
        slot.take();
        true
    }

    pub(super) fn stop(&mut self) -> Result<(), i32> {
        for child in self.slots.iter_mut().flatten() {
            child.release.take();
        }
        let mut error = None;
        for child in self.slots.iter().flatten() {
            // EOF cannot wake a stopped guardian. Resume it after closing the
            // pipe so its existing control loop can clean up the worker tree.
            // Each PID remains ours until wait, including failed assignments.
            if let Err(errno) = linux::signal(child.pid, SIGCONT) {
                if errno != ESRCH {
                    error.get_or_insert(errno);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

// The mutable borrow excludes another admission until this child is recorded.
// Dropping an unused reservation leaves its slot free after fork failure.
pub(super) struct Reservation<'a> {
    slot: &'a mut Option<Guardian>,
}
impl Reservation<'_> {
    pub(super) fn register(self, pid: i32, release: Descriptor) -> Result<(), i32> {
        let child = self.slot.insert(Guardian {
            pid,
            pidfd: None,
            release: None,
        });
        // Record before pidfd_open: on failure the pipe closes, but this child
        // still owns its slot until the broker has waited for it.
        child.pidfd = Some(linux::pidfd(pid)?);
        release.write_all(&[GUARDIAN_START_BYTE])?;
        child.release = Some(release);
        Ok(())
    }
}
