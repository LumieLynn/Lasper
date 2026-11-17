//! Completes the wrapper handoff before the broker admits connections.
//!
//! Ready confirms that the listener exists. The exec-status channel separately
//! confirms that the wrapper has replaced itself with the payload.

use super::{BrokerError, BrokerResult};
use crate::failure::FailureClass;
use crate::linux::signals::Signals;
use crate::linux::uapi::errno::{EINTR, EIO, ETIMEDOUT};
use crate::linux::{self, Descriptor, PollFd};

pub(crate) const STARTUP_TIMEOUT_MS: u64 = 5_000;

/// Broker-side endpoints transferred by the wrapper's fork branch. They must
/// survive inherited-FD pruning. Handoff consumes the two startup channels and
/// retains the parent pidfd for the broker's event loop.
pub(crate) struct Startup {
    /// The broker writes byte 1 after binding the listener, then closes this pipe.
    pub(crate) ready: Descriptor,
    /// The wrapper sends a native-endian i32 errno on exec failure. Successful
    /// exec closes its only endpoint through CLOEXEC, producing EOF here.
    pub(crate) exec_status: Descriptor,
    /// EOF also follows wrapper death. This pidfd distinguishes that case and
    /// remains with the broker to detect payload exit after a successful handoff.
    pub(crate) parent: Descriptor,
}

impl Startup {
    pub(super) fn descriptors(&self) -> [i32; 3] {
        [self.ready.raw(), self.exec_status.raw(), self.parent.raw()]
    }

    pub(super) fn handoff(self, signals: &Signals) -> Result<Descriptor, BrokerError> {
        self.ready.write_all(&[1]).at("report broker Ready")?;
        drop(self.ready);
        let deadline = linux::monotonic_ms()
            .at("read handoff clock")?
            .saturating_add(STARTUP_TIMEOUT_MS);
        loop {
            let remaining =
                deadline.saturating_sub(linux::monotonic_ms().at("read handoff clock")?);
            if remaining == 0 {
                return Err(BrokerError {
                    class: FailureClass::TimedOut,
                    operation: "wait payload exec",
                    errno: ETIMEDOUT,
                });
            }
            let mut events = [
                PollFd::new(self.exec_status.raw()),
                PollFd::new(self.parent.raw()),
                PollFd::new(signals.raw()),
            ];
            match linux::poll(&mut events, Some(remaining)) {
                Ok(()) => {}
                Err(EINTR) => continue,
                Err(errno) => {
                    return Err(BrokerError {
                        class: FailureClass::System,
                        operation: "poll payload handoff",
                        errno,
                    })
                }
            }
            if events[1].ready() || events[2].ready() {
                return Err(BrokerError {
                    class: FailureClass::Unavailable,
                    operation: "wrapper stopped before handoff",
                    errno: EIO,
                });
            }
            if events[0].ready() {
                let mut error = [0; 4];
                let count = self
                    .exec_status
                    .read(&mut error)
                    .at("read payload exec status")?;
                if count != 0 {
                    let errno = i32::from_ne_bytes(error);
                    return Err(BrokerError {
                        class: FailureClass::Exec,
                        operation: "payload exec failed",
                        errno: if count == 4 && (1..=4095).contains(&errno) {
                            errno
                        } else {
                            EIO
                        },
                    });
                }
                // The wrapper could have died since the poll above. Check its
                // lifetime again before treating this EOF as a successful exec.
                let mut parent = [PollFd::new(self.parent.raw())];
                linux::poll(&mut parent, Some(0)).at("check payload lifetime")?;
                if parent[0].ready() {
                    return Err(BrokerError {
                        class: FailureClass::Unavailable,
                        operation: "payload exited during handoff",
                        errno: EIO,
                    });
                }
                return Ok(self.parent);
            }
        }
    }
}
