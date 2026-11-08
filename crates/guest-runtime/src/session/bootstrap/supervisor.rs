use core::cell::Cell;

use crate::linux::uapi::errno::EAGAIN;
use crate::session::error::SystemResultExt;
use crate::session::linux::Descriptor;
use crate::session::GuardianError;

use super::signals::{GuardianSignal, Signals};

pub(super) struct Supervisor {
    broker: Descriptor,
    signals: Signals,
    status: Cell<Result<SupervisorStatus, GuardianError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStatus {
    Alive,
    BrokerDisconnected,
    Stopped(i32),
}

impl Supervisor {
    pub(super) fn new(broker: Descriptor, signals: Signals) -> Self {
        Self {
            broker,
            signals,
            status: Cell::new(Ok(SupervisorStatus::Alive)),
        }
    }

    pub(super) fn descriptors(&self) -> [i32; 2] {
        [self.broker.raw(), self.signals.raw()]
    }

    pub(super) fn receive_signal(&self) -> Result<Option<GuardianSignal>, GuardianError> {
        let signal = match self.signals.receive().context("read guardian signal FD") {
            Ok(signal) => signal,
            Err(error) => {
                if self.status.get() == Ok(SupervisorStatus::Alive) {
                    self.status.set(Err(error));
                }
                return Err(error);
            }
        };
        if let Some(GuardianSignal::Stop(signal)) = signal {
            if self.status.get() == Ok(SupervisorStatus::Alive) {
                self.status.set(Ok(SupervisorStatus::Stopped(signal)));
            }
        }
        Ok(signal)
    }

    // A consumed stop, EOF or protocol error must survive the next observation.
    pub(super) fn status(&self) -> Result<SupervisorStatus, GuardianError> {
        if self.status.get() != Ok(SupervisorStatus::Alive) {
            return self.status.get();
        }
        // Bound signal draining so it cannot postpone broker or deadline checks.
        for _ in 0..8 {
            if self.receive_signal()?.is_none() {
                break;
            }
            if self.status.get() != Ok(SupervisorStatus::Alive) {
                return self.status.get();
            }
        }
        let status = match self.broker.read(&mut [0]) {
            Err(EAGAIN) => Ok(SupervisorStatus::Alive),
            Ok(0) => Ok(SupervisorStatus::BrokerDisconnected),
            Ok(_) => Err(GuardianError::UnexpectedBrokerData),
            Err(errno) => Err(GuardianError::System {
                operation: "read broker liveness",
                errno,
            }),
        };
        self.status.set(status);
        status
    }

    pub(super) fn ensure_alive(&self) -> Result<(), GuardianError> {
        match self.status()? {
            SupervisorStatus::Alive => Ok(()),
            SupervisorStatus::BrokerDisconnected => Err(GuardianError::BrokerDisconnected),
            SupervisorStatus::Stopped(signal) => Err(GuardianError::SupervisorStopped(signal)),
        }
    }
}
