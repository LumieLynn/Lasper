use crate::linux::uapi::errno::EIO;
use core::marker::PhantomData;
use lasper_guest_protocol::{
    ExecutionKind, PreparationId, PrepareReady, PrepareRequestFrame, SessionPayloadError,
    StartRequest,
};

use super::children::{visit_direct_children, Children, CleanupProgress};
use super::error::SystemResultExt;
use super::lifecycle::ExecOutcome;
use super::linux::{self, Descriptor, Pipe, Terminal};

use super::status::{WorkerFailure, WorkerStage, WorkerStatus};
use super::worker::{encode_start, WorkerBootstrap, WorkerChannels, START_BYTES};
use super::{GuardianError, WorkerExit};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerObservation {
    /// Worker account/session setup is complete. Ready delivery and Start
    /// eligibility belong to the guardian's protocol phase.
    pub prepared: bool,
    pub start_requested: bool,
    pub exec: Option<ExecOutcome>,
    pub exit: Option<WorkerExit>,
    pub failure: Option<WorkerFailure>,
    pub error: Option<GuardianError>,
    pub cleanup: CleanupProgress,
}

/// Kernel resources for one guardian's worker. SessionLifecycle decides which
/// operation wins; this owner performs it without replacing the assigned child.
///
/// Polling and cleanup never block on child exit. Drop is a last-resort force
/// cleanup and can wait indefinitely; this type belongs in the dedicated guest
/// guardian, never in the host UI or an async executor.
#[must_use = "the guardian must retain the worker until cleanup is complete"]
pub(super) struct GuardianWorker<'a> {
    request: &'a PrepareRequestFrame<'a>,
    preparation: PreparationId,
    pid: i32,
    pidfd: Option<Descriptor>,
    terminal: Option<Descriptor>,
    identity: Option<Descriptor>,
    stdout: Option<Descriptor>,
    stderr: Option<Descriptor>,
    start: StartDelivery,
    status: WorkerStatus,
    exec: Option<ExecOutcome>,
    exit: Option<WorkerExit>,
    error: Option<GuardianError>,
    children: Children,
    not_send: PhantomData<*mut ()>,
}

enum StartDelivery {
    Waiting(Descriptor),
    Sent,
    Failed,
    Cancelled,
}

impl StartDelivery {
    fn requested(&self) -> bool {
        matches!(self, Self::Sent | Self::Failed)
    }
}

impl<'a> GuardianWorker<'a> {
    /// Forks exactly one worker and obtains its pidfd before any wait/reap.
    /// The entry function is trusted helper code, not a guest executable.
    ///
    /// # Safety
    ///
    /// Call only in a dedicated, single-threaded guardian with no existing
    /// children or other reaper. Entry must use fork-safe operations and end
    /// through exec or process exit. It must not unwind into the fork caller.
    pub unsafe fn spawn(
        request: &'a PrepareRequestFrame<'a>,
        preparation: PreparationId,
        entry: fn(WorkerBootstrap<'_>) -> !,
    ) -> Result<Self, GuardianError> {
        let mut existing = false;
        visit_direct_children(linux::pid(), linux::tid(), |_| {
            existing = true;
            Ok(())
        })
        .context("inspect guardian children")?;
        if existing {
            return Err(GuardianError::ExistingChildren);
        }
        let start = Pipe::new().context("create worker Start pipe")?;
        start
            .writer
            .nonblocking()
            .context("configure worker Start pipe")?;
        let status = Pipe::new().context("create worker status pipe")?;
        status
            .reader
            .nonblocking()
            .context("configure worker status pipe")?;
        let (identity, child_identity) =
            linux::identity_pair().context("create worker identity channel")?;
        let (terminal, stdout, stderr, input, output, error) = match request.request().execution() {
            ExecutionKind::Interactive => {
                let size = request
                    .request()
                    .initial_size()
                    .ok_or(GuardianError::NotPrepared)?;
                let terminal = Terminal::new(size).context("create worker terminal")?;
                (
                    Some(terminal.master),
                    None,
                    None,
                    terminal.slave,
                    None,
                    None,
                )
            }
            ExecutionKind::Bounded => {
                let stdout = Pipe::new().context("create worker stdout")?;
                let stderr = Pipe::new().context("create worker stderr")?;
                stdout
                    .reader
                    .nonblocking()
                    .context("configure worker stdout")?;
                stderr
                    .reader
                    .nonblocking()
                    .context("configure worker stderr")?;
                (
                    None,
                    Some(stdout.reader),
                    Some(stderr.reader),
                    linux::null_device().context("open worker stdin")?,
                    Some(stdout.writer),
                    Some(stderr.writer),
                )
            }
        };
        let child = WorkerChannels {
            start: start.reader,
            status: status.writer,
            identity: child_identity,
            input,
            output,
            error,
        };
        // SAFETY: The caller promises the dedicated guardian context. The
        // child prunes inherited FDs and never returns into this stack frame.
        let pid = unsafe { linux::fork() }.context("fork worker")?;
        if pid == 0 {
            // Entry never returns, so this child's inherited request frame
            // stays alive through exec without copying its inline paths/argv.
            entry(child.bootstrap(request, preparation));
        }
        drop(child);
        // This guardian alone owns wait. Even an early-exit child keeps its
        // PID until we have obtained the assignment handle.
        let (pidfd, fault) = match linux::pidfd(pid) {
            Ok(pidfd) => (Some(pidfd), None),
            Err(errno) => (
                None,
                Some(GuardianError::System {
                    operation: "open assigned worker pidfd",
                    errno,
                }),
            ),
        };
        Ok(Self {
            request,
            preparation,
            pid,
            pidfd,
            terminal,
            identity: Some(identity),
            stdout,
            stderr,
            start: StartDelivery::Waiting(start.writer),
            status: WorkerStatus::new(status.reader),
            exec: None,
            exit: None,
            error: fault,
            children: Children::new(pid),
            not_send: PhantomData,
        })
    }

    pub fn bounded(&self) -> bool {
        self.request.request().execution() == ExecutionKind::Bounded
    }

    pub const fn pid(&self) -> i32 {
        self.pid
    }

    pub const fn cleanup_deadline_ms(&self) -> Option<u64> {
        self.children.next_deadline_ms()
    }

    /// Refreshes status without consuming the child's wait status. Cleanup is
    /// entered on failure or exit, while the original PGID is still pinned.
    pub fn poll(&mut self, now_ms: u64) -> WorkerObservation {
        self.poll_status();
        if self.exit.is_none() && !self.children.worker_reaped() {
            match linux::observe_exit(self.pid) {
                Ok(Some(exit)) => self.exit = Some(exit),
                Ok(None) => {}
                Err(errno) => self.record_error(GuardianError::System {
                    operation: "observe assigned worker exit",
                    errno,
                }),
            }
        }
        // The worker may have written an error between the first pipe read
        // and waitid. Drain it before interpreting the observed exit.
        if self.exit.is_some() {
            self.poll_status();
        }
        self.update_exec_observation();
        if self.error.is_some() || self.status.failure().is_some() || self.exit.is_some() {
            self.begin_cleanup(now_ms);
        }
        self.children.advance(now_ms);
        if self.exit.is_none() {
            if let Some(exit) = self.children.worker_exit() {
                self.exit = Some(exit);
                self.poll_status();
                self.update_exec_observation();
            }
        }
        self.observation()
    }

    fn poll_status(&mut self) {
        if let Err(error) = self.status.poll() {
            self.record_error(error);
        }
        if self.status.session_established() {
            self.children.group_established();
        }
    }

    fn update_exec_observation(&mut self) {
        if self.status.closed()
            && self.status.failure().is_none()
            && self.exit.is_none()
            && (!self.status.prepared() || !self.start.requested())
            && self.children.progress() == CleanupProgress::NotStarted
        {
            self.record_error(GuardianError::InvalidStatus);
        }
        if let Some(WorkerFailure {
            stage: WorkerStage::Exec,
            errno,
        }) = self.status.failure()
        {
            self.exec = Some(ExecOutcome::Failed { errno });
            return;
        }
        if self.exec.is_some() || !self.start.requested() || !self.status.closed() {
            return;
        }
        self.exec = Some(
            if self.status.failure().is_some()
                || self.exit.is_some()
                || !matches!(self.start, StartDelivery::Sent)
                || self.error.is_some()
            {
                ExecOutcome::Unknown
            } else {
                ExecOutcome::Started
            },
        );
    }

    pub fn observation(&self) -> WorkerObservation {
        WorkerObservation {
            prepared: self.setup_ready(),
            start_requested: self.start.requested(),
            exec: self.exec,
            exit: self.exit,
            failure: self.status.failure(),
            error: self.error,
            cleanup: self.children.progress(),
        }
    }

    pub fn ready(
        &mut self,
        lease_ms: u32,
        send: impl FnOnce(lasper_guest_protocol::PrepareReadyFrame, &[i32]) -> Result<(), GuardianError>,
    ) -> Result<lasper_guest_protocol::PrepareReadyFrame, GuardianError> {
        if !self.setup_ready() {
            return Err(GuardianError::NotPrepared);
        }
        let ready = PrepareReady::new(self.preparation, self.request.broker_id(), lease_ms)
            .map_err(|_| GuardianError::NotPrepared)?;
        let frame = self
            .request
            .ready_frame(ready)
            .map_err(GuardianError::PrepareFrame)?;
        let pidfd = self.pidfd.as_ref().ok_or(GuardianError::NotPrepared)?;
        let identity = self.identity.take().ok_or(GuardianError::NotPrepared)?;
        let terminal = self.terminal.take();
        let (fds, count) = match terminal.as_ref() {
            Some(terminal) => ([terminal.raw(), identity.raw(), pidfd.raw()], 3),
            None => ([identity.raw(), pidfd.raw(), -1], 2),
        };
        // The transport duplicates these endpoints before returning. Only its
        // pending sender then owns the export copies; observation keeps pidfd.
        send(frame, &fds[..count])?;
        Ok(frame)
    }

    pub fn validate_start(&self, start: &StartRequest) -> Result<(), GuardianError> {
        if start.broker_id() != self.request.broker_id() {
            return Err(GuardianError::InvalidStart(
                SessionPayloadError::BrokerMismatch,
            ));
        }
        if start.preparation_id() != self.preparation {
            return Err(GuardianError::InvalidStart(
                SessionPayloadError::PreparationMismatch,
            ));
        }
        StartRequest::new(
            self.request,
            self.preparation,
            start.wayland_display(),
            start.x11_display(),
            start.terminal_size(),
        )
        .map_err(GuardianError::InvalidStart)?;
        Ok(())
    }

    /// One small atomic write releases this worker. The pipe is private and
    /// empty: a failed write is reported, never replayed to a replacement child.
    pub fn start(&mut self, start: &StartRequest) -> Result<(), GuardianError> {
        if self.start.requested() {
            return Err(GuardianError::AlreadyStarted);
        }
        if !self.setup_ready() {
            return Err(GuardianError::NotPrepared);
        }
        let StartDelivery::Waiting(pipe) =
            core::mem::replace(&mut self.start, StartDelivery::Failed)
        else {
            return Err(GuardianError::NotPrepared);
        };
        match pipe.write(&encode_start(start)) {
            Ok(START_BYTES) => {
                self.start = StartDelivery::Sent;
                Ok(())
            }
            result => {
                let errno = result.err().unwrap_or(EIO);
                let error = GuardianError::System {
                    operation: "release worker Start",
                    errno,
                };
                self.record_error(error);
                Err(error)
            }
        }
    }

    pub fn begin_cleanup(&mut self, now_ms: u64) {
        if matches!(self.start, StartDelivery::Waiting(_)) {
            self.start = StartDelivery::Cancelled;
        }
        self.children.begin(now_ms);
    }

    pub fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<usize, GuardianError> {
        match &self.stdout {
            Some(fd) => {
                let count = fd.read(buffer).context("read worker stdout")?;
                if count == 0 && !buffer.is_empty() {
                    self.stdout = None;
                }
                Ok(count)
            }
            None => Ok(0),
        }
    }

    pub fn force_cleanup(&mut self) {
        if matches!(self.start, StartDelivery::Waiting(_)) {
            self.start = StartDelivery::Cancelled;
        }
        self.children.force();
    }

    pub fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<usize, GuardianError> {
        match &self.stderr {
            Some(fd) => {
                let count = fd.read(buffer).context("read worker stderr")?;
                if count == 0 && !buffer.is_empty() {
                    self.stderr = None;
                }
                Ok(count)
            }
            None => Ok(0),
        }
    }

    /// Status readiness, assigned-worker exit, and bounded output are all
    /// pollable by the guardian event loop alongside its control/broker FDs.
    pub fn poll_descriptors(&self) -> [i32; 4] {
        [
            self.status.descriptor(),
            if self.exit.is_none() {
                self.pidfd.as_ref().map_or(-1, Descriptor::raw)
            } else {
                -1
            },
            self.stdout.as_ref().map_or(-1, Descriptor::raw),
            self.stderr.as_ref().map_or(-1, Descriptor::raw),
        ]
    }

    pub fn close_output(&mut self) {
        self.stdout.take();
        self.stderr.take();
    }

    fn setup_ready(&self) -> bool {
        self.status.prepared()
            && !self.status.closed()
            && self.pidfd.is_some()
            && matches!(self.start, StartDelivery::Waiting(_))
            && self.status.failure().is_none()
            && self.error.is_none()
            && self.exit.is_none()
            && self.children.progress() == CleanupProgress::NotStarted
    }

    fn record_error(&mut self, error: GuardianError) {
        self.error.get_or_insert(error);
    }
}

impl Drop for GuardianWorker<'_> {
    fn drop(&mut self) {
        let _ = self.status.poll();
        if self.status.session_established() {
            self.children.group_established();
        }
        if matches!(self.start, StartDelivery::Waiting(_)) {
            self.start = StartDelivery::Cancelled;
        }
        self.children.finish_on_drop();
    }
}
