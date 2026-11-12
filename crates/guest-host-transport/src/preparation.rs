use crate::{
    IdentityWorker, MachineTarget, ReceivedControlFrame, UnixIdentityChannel, WorkerObservation,
    WorkerProcess, WorkerReadiness,
};
use lasper_guest_protocol::{PrepareReadyFrame, PrepareRequestFrame};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

/// Resources delivered together by one PrepareReady response. This checks the
/// response correlation and descriptor roles; identity observation still has
/// to establish that the assigned worker sent the reply.
///
/// The guardian is trusted to assign that worker to this preparation and to
/// deliver Start to it. The host checks the resources agree; it cannot recover
/// the guardian's assignment history from a pidfd.
pub struct PreparedWorker<'payload> {
    ready: PrepareReadyFrame,
    target: Arc<MachineTarget>,
    terminal: Option<OwnedFd>,
    identity: IdentityWorker<'payload>,
}

impl<'payload> PreparedWorker<'payload> {
    pub fn from_frame(
        target: Arc<MachineTarget>,
        readiness: &WorkerReadiness,
        request: Arc<PrepareRequestFrame<'payload>>,
        frame: ReceivedControlFrame,
    ) -> io::Result<Self> {
        if request.broker_id() != readiness.broker_id() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PrepareRequest does not belong to the negotiated broker",
            ));
        }
        let (header, payload, descriptors) = frame.into_parts();
        let ready = request
            .decode_ready_parts(header, &payload, descriptors.len())
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid PrepareReady: {error:?}"),
                )
            })?;
        let mut descriptors = descriptors.into_iter();
        let terminal = if ready.descriptor_role(0)
            == Some(lasper_guest_protocol::PrepareReadyDescriptorRole::PtyMaster)
        {
            descriptors.next()
        } else {
            None
        };
        // The protocol decoder has checked the exact count and role order.
        let channel = descriptors.next().expect("PrepareReady identity channel");
        let pidfd = descriptors.next().expect("PrepareReady worker pidfd");
        let process = WorkerProcess::open(pidfd)?;
        target.verify_worker(&process)?;
        if let Some(terminal) = &terminal {
            verify_terminal(terminal.as_fd(), &process)?;
        }
        let channel = UnixIdentityChannel::new(channel, ready)?;
        Ok(Self {
            ready,
            target,
            terminal,
            identity: IdentityWorker::new(channel, request, process),
        })
    }

    pub const fn ready(&self) -> PrepareReadyFrame {
        self.ready
    }

    pub fn terminal(&self) -> Option<BorrowedFd<'_>> {
        self.terminal.as_ref().map(AsFd::as_fd)
    }

    pub(crate) fn into_terminal(self) -> Option<OwnedFd> {
        self.terminal
    }

    pub fn process(&self) -> &WorkerProcess {
        self.identity.process()
    }

    /// No check has failed or been cancelled. This does not replace a fresh
    /// observation of the target and worker before Start.
    pub fn can_continue(&self) -> bool {
        self.identity.can_continue()
    }

    /// Checks the reply and, for interactive workers, the terminal assignment.
    /// The same target instance is checked before and after the reply.
    /// The caller supplies the deadline; cancellation invalidates preparation.
    pub async fn observe(&mut self) -> io::Result<WorkerObservation> {
        let result = async {
            self.verify_resources()?;
            let observation = self.identity.observe().await?;
            self.verify_resources()?;
            Ok(observation)
        }
        .await;
        if result.is_err() {
            self.identity.invalidate();
        }
        result
    }

    pub(crate) fn verify_resources(&self) -> io::Result<()> {
        self.target.verify_worker(self.process())?;
        if let Some(terminal) = &self.terminal {
            verify_terminal(terminal.as_fd(), self.process())?;
        }
        Ok(())
    }
}

fn verify_terminal(terminal: BorrowedFd<'_>, worker: &WorkerProcess) -> io::Result<()> {
    worker.recheck()?;
    let mut number: libc::c_uint = 0;
    // SAFETY: TIOCGPTN writes one unsigned integer and does not alter the PTY.
    if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCGPTN, &mut number) } < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "PrepareReady terminal is not a PTY master: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    // These ioctls return IDs in the caller's PID namespace. During Prepare
    // the worker owns both the session and foreground group; an application
    // may change the foreground group only after Start.
    for request in [libc::TIOCGSID, libc::TIOCGPGRP] {
        let mut owner: libc::pid_t = 0;
        // SAFETY: Both requests write one pid_t into this live output slot.
        if unsafe { libc::ioctl(terminal.as_raw_fd(), request, &mut owner) } < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cannot inspect prepared PTY ownership: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        if owner != worker.pid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared PTY session or foreground group does not belong to the assigned worker",
            ));
        }
    }
    worker.recheck()?;
    Ok(())
}
