use lasper_guest_protocol::{
    ExecutionKind, GuestIdentityReply, GuestProcess, PreparationId, PrepareRequestFrame,
    ProjectionObservations, ProjectionRequests, StartRequest, TerminalSize,
};

use crate::identity::GuestIdentityChannel;
use crate::linux::uapi::errno::{EINTR, EINVAL, EIO};

use super::account::Account;
use super::execution;
use super::linux::{self, Descriptor, PollFd};
use super::memory::WorkerMemory;
use super::status::{StatusMessage, WorkerFailure, WorkerStage};

pub(super) struct WorkerChannels {
    pub(super) start: Descriptor,
    pub(super) status: Descriptor,
    pub(super) identity: Descriptor,
    pub(super) input: Descriptor,
    pub(super) output: Option<Descriptor>,
    pub(super) error: Option<Descriptor>,
}

impl WorkerChannels {
    pub(super) fn bootstrap<'a>(
        self,
        request: &'a PrepareRequestFrame<'a>,
        preparation: PreparationId,
    ) -> WorkerBootstrap<'a> {
        let retained = [
            self.start.raw(),
            self.status.raw(),
            self.identity.raw(),
            self.input.raw(),
            self.output.as_ref().map_or(-1, Descriptor::raw),
            self.error.as_ref().map_or(-1, Descriptor::raw),
        ];
        let setup = || {
            linux::close_unrelated(&retained)
                .map_err(|errno| WorkerFailure::new(WorkerStage::Descriptors, errno))?;
            linux::worker_signals()
                .map_err(|errno| WorkerFailure::new(WorkerStage::Signals, errno))?;
            let terminal = self.output.is_none();
            linux::setup_session(terminal.then_some(&self.input))
                .map_err(|errno| WorkerFailure::new(WorkerStage::Session, errno))?;
            self.input
                .duplicate_to(0)
                .and_then(|()| self.output.as_ref().unwrap_or(&self.input).duplicate_to(1))
                .and_then(|()| self.error.as_ref().unwrap_or(&self.input).duplicate_to(2))
                .map_err(|errno| WorkerFailure::new(WorkerStage::StandardIo, errno))
        };
        if let Err(failure) = setup() {
            fail(&self.status, failure);
        }
        drop(self.input);
        drop(self.output);
        drop(self.error);
        if StatusMessage::SessionEstablished
            .send(&self.status)
            .is_err()
        {
            linux::exit(126);
        }
        // SAFETY: Only this child owns the worker endpoint after descriptor pruning.
        let identity =
            unsafe { GuestIdentityChannel::from_raw_fd(self.identity.into_raw(), preparation) };
        WorkerBootstrap {
            request,
            preparation,
            start: self.start,
            status: self.status,
            identity,
        }
    }
}

/// The trusted worker entry calls `run` to prepare the account and wait for
/// Start. It can report a setup failure, but cannot release an unchecked exec.
pub struct WorkerBootstrap<'a> {
    request: &'a PrepareRequestFrame<'a>,
    preparation: PreparationId,
    start: Descriptor,
    status: Descriptor,
    identity: GuestIdentityChannel,
}

impl<'a> WorkerBootstrap<'a> {
    pub const fn request(&self) -> &PrepareRequestFrame<'a> {
        self.request
    }

    pub fn fail(self, failure: WorkerFailure) -> ! {
        fail(&self.status, failure)
    }

    /// Resolves local passwd/group, switches identity and enters home before
    /// publishing Prepared. The observer runs under that selected identity.
    ///
    /// The worker owns one WORKER_MEMORY_BYTES mapping for its account and
    /// buffers. exec/exit releases it; entries do not allocate a second workspace.
    pub fn run(
        self,
        mut observe: impl FnMut(&ProjectionRequests) -> Result<ProjectionObservations, WorkerFailure>,
    ) -> ! {
        let mut memory = match WorkerMemory::new() {
            Ok(memory) => memory,
            Err(errno) => self.fail(WorkerFailure::new(WorkerStage::Memory, errno)),
        };
        let storage = memory.storage();
        let account = Account::initialize(&mut storage.account);
        if let Err(failure) = account.load(self.request.request().user(), &mut storage.scratch) {
            self.fail(failure);
        }
        if let Err(failure) = account.enter(
            self.request.request().execution() == ExecutionKind::Interactive,
            matches!(self.request.request().process(), GuestProcess::LoginShell),
        ) {
            self.fail(failure);
        }
        let started = self.await_start(account, &mut storage.packet, &mut observe);
        let failure = execution::execute(
            account,
            started.request.request(),
            &started.start,
            &mut storage.scratch,
            &mut storage.packet,
        );
        fail(&started.status, failure)
    }

    fn await_start(
        mut self,
        account: &Account,
        packet: &mut [u8],
        observe: &mut impl FnMut(&ProjectionRequests) -> Result<ProjectionObservations, WorkerFailure>,
    ) -> StartedWorker<'a> {
        if let Err(errno) = StatusMessage::Prepared.send(&self.status) {
            fail(&self.status, WorkerFailure::new(WorkerStage::Start, errno));
        }
        let identity_fd = self.identity_descriptor();
        loop {
            let mut fds = [PollFd::new(self.start.raw()), PollFd::new(identity_fd)];
            if let Err(errno) = linux::poll(&mut fds, None) {
                if errno == EINTR {
                    continue;
                }
                fail(&self.status, WorkerFailure::new(WorkerStage::Start, errno));
            }
            if fds[0].ready() {
                let mut command = [0; START_BYTES];
                let mut filled = 0;
                while filled < command.len() {
                    match self.start.read(&mut command[filled..]) {
                        Ok(0) => linux::exit(125),
                        Ok(count) => filled += count,
                        Err(errno) => {
                            fail(&self.status, WorkerFailure::new(WorkerStage::Start, errno))
                        }
                    }
                }
                let start = match decode_start(self.request, self.preparation, command) {
                    Ok(start) => start,
                    Err(failure) => fail(&self.status, failure),
                };
                if let Some(size) = start.terminal_size() {
                    if let Err(errno) = linux::set_terminal_size(0, size) {
                        fail(&self.status, WorkerFailure::new(WorkerStage::Start, errno));
                    }
                }
                drop(self.identity);
                drop(self.start);
                return StartedWorker {
                    request: self.request,
                    start,
                    status: self.status,
                };
            }
            if fds[1].ready() {
                let challenge = match self.identity.receive_challenge(packet) {
                    Ok(challenge) => challenge,
                    Err(_) => fail(
                        &self.status,
                        WorkerFailure::new(WorkerStage::Projection, EIO),
                    ),
                };
                let projections = match observe(self.request.request().projections()) {
                    Ok(projections) => projections,
                    Err(failure) => fail(&self.status, failure),
                };
                let reply = match GuestIdentityReply::new(
                    challenge,
                    self.request.request(),
                    account.username.as_str(),
                    account.uid,
                    account.gid,
                    projections,
                ) {
                    Ok(reply) => reply,
                    Err(_) => fail(
                        &self.status,
                        WorkerFailure::new(WorkerStage::Projection, EINVAL),
                    ),
                };
                if self.identity.send_reply(&reply, packet).is_err() {
                    fail(
                        &self.status,
                        WorkerFailure::new(WorkerStage::Projection, EIO),
                    );
                }
            }
        }
    }

    fn identity_descriptor(&self) -> i32 {
        self.identity.as_raw_fd()
    }
}

// The status pipe survives Start. exec closes it; setup or exec failure sends
// an error before the worker exits.
struct StartedWorker<'a> {
    request: &'a PrepareRequestFrame<'a>,
    start: StartRequest,
    status: Descriptor,
}

pub(super) const START_BYTES: usize = 8;

// Paths and account/argv cannot change at Start. This private pipe carries only
// projection selection and dimensions; the worker uses its own original Prepare.
pub(super) fn encode_start(start: &StartRequest) -> [u8; START_BYTES] {
    let mut bytes = [0; START_BYTES];
    bytes[0] = u8::from(start.wayland_display().is_some())
        | (u8::from(start.x11_display().is_some()) << 1);
    if let Some(size) = start.terminal_size() {
        bytes[1] = 1;
        bytes[2..4].copy_from_slice(&size.cols().to_le_bytes());
        bytes[4..6].copy_from_slice(&size.rows().to_le_bytes());
    }
    bytes
}

fn decode_start(
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    bytes: [u8; START_BYTES],
) -> Result<StartRequest, WorkerFailure> {
    let invalid = WorkerFailure::new(WorkerStage::Start, EINVAL);
    if bytes[0] & !3 != 0 || bytes[1] > 1 || bytes[6..] != [0, 0] {
        return Err(invalid);
    }
    let size = if bytes[1] == 1 {
        Some(
            TerminalSize::new(
                u16::from_le_bytes([bytes[2], bytes[3]]),
                u16::from_le_bytes([bytes[4], bytes[5]]),
            )
            .map_err(|_| invalid)?,
        )
    } else {
        if bytes[2..6] != [0; 4] {
            return Err(invalid);
        }
        None
    };
    let projections = request.request().projections();
    let wayland = if bytes[0] & 1 != 0 {
        Some(projections.wayland().ok_or(invalid)?.target())
    } else {
        None
    };
    let x11 = if bytes[0] & 2 != 0 {
        Some(projections.x11().ok_or(invalid)?.display())
    } else {
        None
    };
    StartRequest::new(request, preparation, wayland, x11, size).map_err(|_| invalid)
}

fn fail(status: &Descriptor, failure: WorkerFailure) -> ! {
    let _ = StatusMessage::Failed(failure).send(status);
    linux::exit(126)
}
