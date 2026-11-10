use crate::linux::children::visit_direct_children;

use super::WorkerExit;
use crate::linux;
use crate::linux::uapi::{
    errno::{ECHILD, ESRCH},
    signal::{SIGHUP, SIGKILL, SIGTERM},
};

const GRACE_MS: u64 = 1_000;
const REAP_BUDGET: usize = 256;
const CLEANUP_RETRY_MS: u64 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupProgress {
    NotStarted,
    Terminating,
    Pending { errno: Option<i32> },
    Complete,
}

pub(super) struct Children {
    guardian_pid: i32,
    guardian_tid: i32,
    worker_pid: i32,
    group_established: bool,
    worker_reaped: bool,
    worker_exit: Option<WorkerExit>,
    termination: Option<Termination>,
    next_poll_ms: Option<u64>,
    progress: CleanupProgress,
}

enum Termination {
    Grace { until_ms: u64 },
    Force,
}

impl Children {
    pub(super) fn new(worker_pid: i32) -> Self {
        Self {
            guardian_pid: linux::pid(),
            guardian_tid: linux::tid(),
            worker_pid,
            group_established: false,
            worker_reaped: false,
            worker_exit: None,
            termination: None,
            next_poll_ms: None,
            progress: CleanupProgress::NotStarted,
        }
    }

    pub(super) const fn progress(&self) -> CleanupProgress {
        self.progress
    }

    pub(super) const fn worker_reaped(&self) -> bool {
        self.worker_reaped
    }

    pub(super) const fn worker_exit(&self) -> Option<WorkerExit> {
        self.worker_exit
    }

    pub(super) const fn next_deadline_ms(&self) -> Option<u64> {
        self.next_poll_ms
    }

    pub(super) fn group_established(&mut self) {
        self.group_established = true;
    }

    pub(super) fn begin(&mut self, now_ms: u64) {
        if self.termination.is_none() {
            self.termination = Some(Termination::Grace {
                until_ms: now_ms.saturating_add(GRACE_MS),
            });
            self.next_poll_ms = Some(now_ms);
            self.progress = CleanupProgress::Terminating;
        }
    }

    pub(super) fn force(&mut self) {
        if self.progress != CleanupProgress::Complete {
            self.termination = Some(Termination::Force);
            self.next_poll_ms = None;
            self.progress = CleanupProgress::Terminating;
        }
    }

    pub(super) fn advance(&mut self, now_ms: u64) -> CleanupProgress {
        let Some(termination) = &self.termination else {
            return self.progress;
        };
        if self.progress == CleanupProgress::Complete {
            return self.progress;
        }
        let (force, deadline) = match termination {
            Termination::Grace { until_ms } => (now_ms >= *until_ms, *until_ms),
            Termination::Force => (true, now_ms),
        };
        let signal = if force { SIGKILL } else { SIGTERM };
        let mut error = None;
        if !self.worker_reaped {
            ignore_missing(linux::signal(self.worker_pid, signal), &mut error);
        }
        // The unreaped worker anchors its PGID, including after it has exited.
        // Once reaped, only the guardian's direct-child relation is used.
        if self.group_established && !self.worker_reaped {
            if !force {
                ignore_missing(linux::signal(-self.worker_pid, SIGHUP), &mut error);
            }
            ignore_missing(linux::signal(-self.worker_pid, signal), &mut error);
        }
        let result =
            visit_direct_children(
                self.guardian_pid,
                self.guardian_tid,
                |pid| match linux::signal(pid, signal) {
                    Ok(()) | Err(ESRCH) => Ok(()),
                    Err(errno) => {
                        error.get_or_insert(errno);
                        Ok(())
                    }
                },
            );
        if let Err(errno) = result {
            error.get_or_insert(errno);
        }

        if force {
            for _ in 0..REAP_BUDGET {
                match linux::reap_one() {
                    Ok(Some(child)) => {
                        if child.pid == self.worker_pid {
                            self.worker_reaped = true;
                            self.worker_exit = Some(child.exit);
                        }
                    }
                    Ok(None) => break,
                    Err(ECHILD) => {
                        self.progress = CleanupProgress::Complete;
                        self.next_poll_ms = None;
                        return self.progress;
                    }
                    Err(errno) => {
                        error.get_or_insert(errno);
                        break;
                    }
                }
            }
        }
        self.progress = match error {
            Some(errno) => CleanupProgress::Pending { errno: Some(errno) },
            None if force => CleanupProgress::Pending { errno: None },
            None => CleanupProgress::Terminating,
        };
        self.next_poll_ms = Some(if force {
            now_ms.saturating_add(CLEANUP_RETRY_MS)
        } else {
            deadline
        });
        self.progress
    }

    pub(super) fn finish_on_drop(&mut self) {
        if self.progress == CleanupProgress::Complete {
            return;
        }
        self.force();
        loop {
            if self.advance(0) == CleanupProgress::Complete {
                return;
            }
            // Revisit adopted children after each reap. Blocking in wait here
            // could miss a newly adopted child that has not been signalled yet.
            let _ = linux::poll(&mut [], Some(10));
        }
    }
}

fn ignore_missing(result: Result<(), i32>, error: &mut Option<i32>) {
    if let Err(errno) = result {
        if errno != ESRCH {
            error.get_or_insert(errno);
        }
    }
}
