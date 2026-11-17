//! Child-side setup and the private Start gate.
//!
//! Account resolution, credentials, argv and environment are fixed before the
//! worker reports Prepared. While waiting for Start, it answers identity
//! challenges as the selected account. Only the guardian can release its exec.

use lasper_guest_protocol::{
    ExecutionKind, GuestIdentityReply, GuestProcess, PreparationId, PrepareRequestFrame,
    StartRequest, TerminalSize,
};

use crate::identity::{GuestIdentityChannel, IdentityTransportError};
use crate::linux::uapi::errno::{EINTR, EINVAL, EIO, EPIPE};

use super::account::Account;
use super::execution;
use super::memory::WorkerMemory;
use super::status::{StatusMessage, WorkerFailure, WorkerStage};
use crate::linux::{self, Descriptor, PollFd};

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
        profile: &'static lasper_guest_protocol::ProtocolProfile,
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
            linux::exit(crate::failure::FailureClass::Io.exit_code());
        }
        // SAFETY: Only this child owns the worker endpoint after descriptor pruning.
        let identity = unsafe {
            GuestIdentityChannel::from_raw_fd(self.identity.into_raw(), preparation, profile)
        };
        WorkerBootstrap {
            request,
            preparation,
            profile,
            start: self.start,
            status: self.status,
            identity,
        }
    }
}

/// Owns worker-side channels after inherited FDs have been pruned. `run`
/// prepares execution and answers identity challenges until Start arrives.
pub struct WorkerBootstrap<'a> {
    request: &'a PrepareRequestFrame<'a>,
    preparation: PreparationId,
    profile: &'static lasper_guest_protocol::ProtocolProfile,
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
    /// publishing Prepared. Identity challenges are answered under that identity.
    ///
    /// The worker owns one WORKER_MEMORY_BYTES mapping for its account and
    /// buffers until exec/exit. Environment decoding and merging use a separate
    /// mapping sized by the selected profile. Both are ready before Prepared.
    pub fn run(self) -> ! {
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
            self.request.request().execution() == ExecutionKind::Terminal,
            matches!(self.request.request().process(), GuestProcess::LoginShell),
        ) {
            self.fail(failure);
        }
        let mut execution_memory = match execution::ExecutionMemory::new(self.profile.capacities())
        {
            Ok(memory) => memory,
            Err(failure) => self.fail(failure),
        };
        let execution =
            match execution_memory.prepare(account, self.request.request(), &mut storage.scratch) {
                Ok(execution) => execution,
                Err(failure) => self.fail(failure),
            };
        let status = self.await_start(account, &mut storage.packet);
        fail(&status, execution.execute())
    }

    fn await_start(mut self, account: &Account, packet: &mut [u8]) -> Descriptor {
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
                        Ok(0) => fail(&self.status, WorkerFailure::new(WorkerStage::Start, EPIPE)),
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
                // exec closes the CLOEXEC status pipe; a failed exec reports
                // its errno through the same endpoint before exiting.
                return self.status;
            }
            if fds[1].ready() {
                let challenge = match self.identity.receive_challenge(packet) {
                    Ok(challenge) => challenge,
                    Err(error) => fail(&self.status, identity_failure(error)),
                };
                let reply = match GuestIdentityReply::new(
                    challenge,
                    self.request.request(),
                    account.username.as_str(),
                    account.uid,
                    account.gid,
                ) {
                    Ok(reply) => reply,
                    Err(_) => fail(
                        &self.status,
                        WorkerFailure::new(WorkerStage::Identity, EINVAL),
                    ),
                };
                if let Err(error) = self.identity.send_reply(&reply, packet) {
                    fail(&self.status, identity_failure(error));
                }
            }
        }
    }

    fn identity_descriptor(&self) -> i32 {
        self.identity.as_raw_fd()
    }
}

pub(super) const START_BYTES: usize = 8;

// Paths and account/argv cannot change at Start. This private pipe carries only
// final terminal dimensions; the worker uses its own original Prepare.
pub(super) fn encode_start(start: &StartRequest) -> [u8; START_BYTES] {
    let mut bytes = [0; START_BYTES];
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
    if bytes[0] != 0 || bytes[1] > 1 || bytes[6..] != [0, 0] {
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
    StartRequest::new(request, preparation, size, None).map_err(|_| invalid)
}

fn fail(status: &Descriptor, failure: WorkerFailure) -> ! {
    let _ = StatusMessage::Failed(failure).send(status);
    linux::exit(failure.class().exit_code())
}

fn identity_failure(error: IdentityTransportError) -> WorkerFailure {
    let errno = match error {
        IdentityTransportError::Syscall(errno) => errno,
        IdentityTransportError::UnexpectedEof | IdentityTransportError::Poisoned => EPIPE,
        IdentityTransportError::Payload(_)
        | IdentityTransportError::Truncated
        | IdentityTransportError::UnexpectedAncillary
        | IdentityTransportError::PreparationMismatch
        | IdentityTransportError::ChallengeMismatch => EINVAL,
        IdentityTransportError::WriteZero | IdentityTransportError::UnexpectedPhase => EIO,
    };
    WorkerFailure::new(WorkerStage::Identity, errno)
}

#[cfg(test)]
#[path = "../../tests/support/clock_worker.rs"]
pub(super) mod clock_fixture;

#[cfg(test)]
#[path = "../../tests/session/worker.rs"]
mod tests;
