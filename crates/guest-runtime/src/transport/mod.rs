mod ancillary;
mod descriptors;
mod receive;
mod send;

use core::cell::Cell;
use lasper_guest_protocol::{
    ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, ControlSessionError,
    ControlSessionState, EnforcedControlLimits,
};

use crate::linux::uapi::errno::{EAGAIN, EINTR};
use crate::socket::{close, shutdown};

pub use descriptors::ReceivedDescriptors;
pub use receive::{FrameReceiver, GuestReceivedFrame, PayloadReceiver};
pub use send::FrameSender;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    InvalidFrame(ControlFrameError),
    InvalidHeader(ControlFrameHeaderError),
    Session(ControlSessionError),
    PayloadBufferTooSmall { required: usize, available: usize },
    DescriptorCountMismatch { declared: u16, actual: usize },
    UnexpectedDescriptors,
    IncompleteFrame,
    Truncated,
    UnexpectedEof,
    WriteZero,
    ReceiveInProgress,
    SendInProgress,
    Syscall(i32),
    Poisoned,
}

/// A guest control connection driven by the guardian's event loop.
///
/// Each operation retains its byte offset and FDs across Pending. Dropping an
/// operation after any bytes crossed the socket closes the connection, since
/// the next operation cannot safely start at that partial frame's tail.
/// One reader and one writer may coexist on the single guardian thread; a
/// pending operation borrows only this connection, never the worker lifecycle.
pub struct GuestControlStream {
    fd: i32,
    poisoned: Cell<bool>,
    session: Cell<ControlSessionState>,
    receiving: Cell<bool>,
    sending: Cell<bool>,
}

impl GuestControlStream {
    /// Takes ownership of a connected AF_UNIX stream descriptor. Individual
    /// syscalls use MSG_DONTWAIT; this does not change the FD's shared flags.
    ///
    /// # Safety
    ///
    /// The caller must transfer exclusive ownership of a live connected FD.
    pub unsafe fn from_raw_fd(fd: i32) -> Self {
        Self {
            fd,
            poisoned: Cell::new(false),
            session: Cell::new(ControlSessionState::guest()),
            receiving: Cell::new(false),
            sending: Cell::new(false),
        }
    }

    pub const fn as_raw_fd(&self) -> i32 {
        self.fd
    }

    /// Install the negotiate result from the Hello pair exchanged on this
    /// connection. The transport checks the phase, not the origin of limits.
    pub fn complete_hello(&mut self, limits: EnforcedControlLimits) -> Result<(), TransportError> {
        self.ensure_usable()?;
        let mut session = self.session.get();
        session
            .complete_hello(limits)
            .map_err(TransportError::Session)?;
        self.session.set(session);
        Ok(())
    }

    /// False after a failed or abandoned partial operation. Pending operations
    /// can continue only through their existing receiver/sender, never a new one.
    pub fn can_continue(&self) -> bool {
        !self.poisoned.get()
    }

    pub fn begin_receive(&self) -> Result<FrameReceiver<'_>, TransportError> {
        self.ensure_usable()?;
        FrameReceiver::new(self)
    }

    /// Validation and FD duplication precede I/O. The sender owns its FD copies
    /// until the kernel accepts the first byte, or until the operation is dropped.
    pub fn begin_send<B: AsRef<[u8]>>(
        &self,
        header: ControlFrameHeader,
        payload: B,
        descriptors: &[i32],
    ) -> Result<FrameSender<'_, B>, TransportError> {
        self.ensure_usable()?;
        FrameSender::new(self, header, payload, descriptors)
    }

    fn ensure_usable(&self) -> Result<(), TransportError> {
        if self.poisoned.get() {
            Err(TransportError::Poisoned)
        } else {
            Ok(())
        }
    }

    pub(crate) fn poison(&self) {
        self.poisoned.set(true);
        let _ = shutdown(self.fd);
    }
}

impl Drop for GuestControlStream {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

struct Operation<'a> {
    stream: &'a GuestControlStream,
    direction: Direction,
    started: bool,
    finished: bool,
}

impl<'a> Operation<'a> {
    fn new(stream: &'a GuestControlStream, direction: Direction) -> Result<Self, TransportError> {
        let busy = match direction {
            Direction::Receive => &stream.receiving,
            Direction::Send => &stream.sending,
        };
        if busy.replace(true) {
            return Err(match direction {
                Direction::Receive => TransportError::ReceiveInProgress,
                Direction::Send => TransportError::SendInProgress,
            });
        }
        Ok(Self {
            stream,
            direction,
            started: false,
            finished: false,
        })
    }

    fn finish(&mut self, header: ControlFrameHeader) -> Result<(), TransportError> {
        let session = self.stream.session.get();
        let session = match self.direction {
            Direction::Receive => session.next_inbound(header),
            Direction::Send => session.next_outbound(header),
        }
        .map_err(TransportError::Session)?;
        self.stream.session.set(session);
        self.finished = true;
        Ok(())
    }

    fn fail(&mut self, error: TransportError) -> TransportError {
        self.stream.poison();
        error
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if self.started && !self.finished {
            self.stream.poison();
        }
        match self.direction {
            Direction::Receive => self.stream.receiving.set(false),
            Direction::Send => self.stream.sending.set(false),
        }
    }
}

enum Direction {
    Receive,
    Send,
}

// Limit syscall attempts as well as bytes so repeated EINTR cannot keep the
// guardian from checking its deadlines and child/liveness descriptors.
pub(crate) const IO_BYTES_PER_TURN: usize = 64 * 1024;

pub struct IoBudget {
    bytes: usize,
    calls: usize,
}

impl IoBudget {
    pub fn new() -> Self {
        Self::with_bytes(IO_BYTES_PER_TURN)
    }

    /// A smaller turn budget shares the same syscall bound and never raises
    /// the transport's 64 KiB ceiling.
    pub fn with_bytes(bytes: usize) -> Self {
        Self {
            bytes: bytes.min(IO_BYTES_PER_TURN),
            calls: 8,
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.bytes != 0 && self.calls != 0
    }
    pub(crate) fn length(&self, remaining: usize) -> usize {
        remaining.min(self.bytes)
    }
    pub(crate) fn attempted(&mut self) {
        self.calls -= 1;
    }
    pub(crate) fn transferred(&mut self, bytes: usize) {
        self.bytes -= bytes;
    }
}

impl Default for IoBudget {
    fn default() -> Self {
        Self::new()
    }
}
