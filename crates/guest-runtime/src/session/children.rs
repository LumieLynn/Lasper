use core::ffi::CStr;
use core::fmt::{self, Write};

use super::linux::{self, Descriptor, ECHILD, EIO, ESRCH, SIGHUP, SIGKILL, SIGTERM};
use super::WorkerExit;

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
    deadline_ms: Option<u64>,
    next_poll_ms: Option<u64>,
    progress: CleanupProgress,
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
            deadline_ms: None,
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
        if self.deadline_ms.is_none() {
            self.deadline_ms = Some(now_ms.saturating_add(GRACE_MS));
            self.next_poll_ms = Some(now_ms);
            self.progress = CleanupProgress::Terminating;
        }
    }

    pub(super) fn advance(&mut self, now_ms: u64) -> CleanupProgress {
        let Some(deadline) = self.deadline_ms else {
            return self.progress;
        };
        if self.progress == CleanupProgress::Complete {
            return self.progress;
        }
        let force = now_ms >= deadline;
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
        self.begin(0);
        loop {
            if self.advance(u64::MAX) == CleanupProgress::Complete {
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

pub(super) fn visit_direct_children(
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
mod tests {
    use super::*;

    #[test]
    fn proc_stat_parent_handles_spaces_and_parentheses_in_comm() {
        assert_eq!(stat_parent(b"20 (an odd ) name) S 17 20 20 0\n"), Some(17));
        assert_eq!(stat_parent(b"20 (missing state)\n"), None);
    }
}
