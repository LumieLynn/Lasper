use core::task::Poll;
use lasper_guest_protocol::{
    ErrorFrame, GuestErrorCode, GuestOperation, MessageKind, PreparationId, PrepareRequestFrame,
};

use super::super::error::SystemResultExt;
use super::super::lifecycle::WORKER_SETUP_TIMEOUT_MS;
use super::super::ready::IO_TIMEOUT_MS;
use super::super::{
    GuardianContext, GuardianError, PrepareBuffer, PrepareStorage, PrepareStorageError,
    SessionGuardian, SupervisorStatus,
};
use super::capture::Capture;
use crate::linux::uapi::{
    errno::{EINTR, EIO},
    nr,
    poll::{POLLIN, POLLOUT},
    random::GRND_NONBLOCK,
};
use crate::linux::{self, PollFd};
use crate::syscall::syscall3;
use crate::transport::TransportError;

const PREPARE_RECEIVE_TIMEOUT_MS: u64 = 5_000;

pub(super) fn receive(context: &GuardianContext) -> Result<PrepareStorage, GuardianError> {
    let deadline = context
        .hello_completed_at_ms()
        .saturating_add(PREPARE_RECEIVE_TIMEOUT_MS);
    let connection = context.connection();
    let mut reader = connection.begin_receive().map_err(GuardianError::Control)?;
    let header = drive(
        context,
        deadline,
        GuardianError::PrepareExpired,
        POLLIN,
        || reader.advance(),
    )?;
    if header.kind() != MessageKind::PrepareRequest || header.descriptor_count() != 0 {
        return Err(GuardianError::InvalidStatus);
    }
    if header.request_id() <= context.hello_request_id() {
        return Err(reject(
            context,
            header.request_id(),
            GuardianError::InvalidStatus,
        ));
    }
    let buffer = match PrepareBuffer::new(header, context.broker_id(), context.negotiated_limits())
    {
        Ok(buffer) => buffer,
        Err(error) => {
            // Keep the header reader alive until the rejection is sent. Dropping
            // it first would poison this still-partial inbound frame.
            return Err(reject(
                context,
                header.request_id(),
                GuardianError::PrepareStorage(error),
            ));
        }
    };
    let mut reader = reader
        .into_payload(buffer)
        .map_err(GuardianError::Control)?;
    if let Err(error) = drive(
        context,
        deadline,
        GuardianError::PrepareExpired,
        POLLIN,
        || reader.advance(),
    ) {
        return Err(reject(context, header.request_id(), error));
    }
    let frame = reader.finish().map_err(GuardianError::Control)?;
    PrepareStorage::from_received(frame).map_err(|error| {
        reject(
            context,
            header.request_id(),
            GuardianError::PrepareStorage(error),
        )
    })
}

pub(super) fn assign<'request, 'context>(
    context: &'context GuardianContext,
    request: &'request PrepareRequestFrame<'request>,
    accepted_at: u64,
    entry: fn(super::super::WorkerBootstrap<'_>) -> !,
) -> Result<
    (
        PreparationId,
        SessionGuardian<'request, 'context>,
        Option<Capture>,
    ),
    GuardianError,
> {
    let capture = if request.request().execution() == lasper_guest_protocol::ExecutionKind::Bounded
    {
        Some(Capture::new(context.negotiated_limits().get())?)
    } else {
        None
    };
    let preparation = preparation_id()?;
    check(
        context,
        accepted_at.saturating_add(WORKER_SETUP_TIMEOUT_MS),
        GuardianError::PrepareExpired,
    )?;
    let guardian = context.spawn(request, preparation, accepted_at, entry)?;
    Ok((preparation, guardian, capture))
}

fn check(
    context: &GuardianContext,
    deadline: u64,
    expired: GuardianError,
) -> Result<u64, GuardianError> {
    match context.supervisor_status()? {
        SupervisorStatus::Alive => {}
        SupervisorStatus::BrokerDisconnected => return Err(GuardianError::BrokerDisconnected),
        SupervisorStatus::Stopped(signal) => return Err(GuardianError::SupervisorStopped(signal)),
    }
    let now = linux::monotonic_ms().context("read guardian preparation clock")?;
    if now >= deadline {
        return Err(expired);
    }
    Ok(now)
}

fn drive<T>(
    context: &GuardianContext,
    deadline: u64,
    expired: GuardianError,
    events: i16,
    mut advance: impl FnMut() -> Result<Poll<T>, TransportError>,
) -> Result<T, GuardianError> {
    loop {
        check(context, deadline, expired)?;
        match advance().map_err(GuardianError::Control)? {
            Poll::Ready(value) => {
                check(context, deadline, expired)?;
                return Ok(value);
            }
            Poll::Pending => {
                let now = check(context, deadline, expired)?;
                let [control, broker, signals] = context.poll_descriptors();
                let mut descriptors = [
                    PollFd::new(control),
                    PollFd::new(broker),
                    PollFd::new(signals),
                ];
                descriptors[0].events = events;
                match linux::poll(&mut descriptors, Some(deadline - now)) {
                    Err(EINTR) => {}
                    result => result.context("wait for guardian preparation")?,
                }
            }
        }
    }
}

fn preparation_id() -> Result<PreparationId, GuardianError> {
    let mut bytes = [0; 16];
    let mut offset = 0;
    while offset < bytes.len() {
        // SAFETY: The kernel fills only the remaining live nonce bytes.
        let result = unsafe {
            syscall3(
                nr::GETRANDOM,
                bytes[offset..].as_mut_ptr() as usize,
                bytes.len() - offset,
                GRND_NONBLOCK,
            )
        };
        if result == -(EINTR as isize) {
            continue;
        }
        let count = linux::result_or_errno(result).context("generate preparation ID")?;
        if count == 0 {
            return Err(GuardianError::System {
                operation: "generate preparation ID",
                errno: EIO,
            });
        }
        offset += count;
    }
    Ok(PreparationId::new(bytes))
}

pub(super) fn reject(context: &GuardianContext, id: u64, error: GuardianError) -> GuardianError {
    // A failed rejection cannot recover this one-request connection. Preserve
    // the preparation error locally even when its reply cannot be delivered.
    let _ = send_error(context, id, error);
    error
}

fn send_error(
    context: &GuardianContext,
    id: u64,
    error: GuardianError,
) -> Result<(), GuardianError> {
    let (code, errno) = match error {
        GuardianError::System { errno, .. }
        | GuardianError::PrepareStorage(PrepareStorageError::System(errno)) => {
            (GuestErrorCode::System, Some(errno))
        }
        GuardianError::PrepareExpired => (GuestErrorCode::Expired, None),
        GuardianError::InvalidStatus
        | GuardianError::PrepareStorage(PrepareStorageError::Frame(_)) => {
            (GuestErrorCode::InvalidRequest, None)
        }
        _ => (GuestErrorCode::Unavailable, None),
    };
    let error = ErrorFrame::new(
        id,
        context.broker_id(),
        None,
        GuestOperation::Prepare,
        code,
        errno,
    )
    .map_err(|_| GuardianError::ReplyEncoding)?;
    // Rejection must still work when the Prepare/output mapping could not be allocated.
    let mut bytes = [0; 512];
    let (header, length) = error
        .encode_parts(&mut bytes)
        .map_err(GuardianError::SessionFrame)?;
    let mut sender = context
        .connection()
        .begin_send(header, &bytes[..length], &[])
        .map_err(GuardianError::Control)?;
    let now = match linux::monotonic_ms().context("read guardian rejection clock") {
        Ok(now) => now,
        Err(error) => {
            // Without a clock, make one bounded, nonblocking attempt; never wait.
            return match sender.advance().map_err(GuardianError::Control)? {
                Poll::Ready(()) => Ok(()),
                Poll::Pending => Err(error),
            };
        }
    };
    drive(
        context,
        now.saturating_add(IO_TIMEOUT_MS),
        GuardianError::ControlExpired,
        POLLOUT,
        || sender.advance(),
    )
}
