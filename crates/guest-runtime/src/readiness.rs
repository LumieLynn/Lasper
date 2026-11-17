use crate::linux::uapi::{errno::EINTR, nr};
use crate::socket::close;
use crate::syscall::{syscall1, syscall2};
use core::fmt;
use lasper_guest_protocol::{
    BrokerId, CompatibilityError, ControlLimits, EnforcedControlLimits, HelloRequest,
    HelloResponse, ProtocolVersion,
};

/// Evidence that this process can open pidfds. Broker startup and each
/// guardian's Hello exchange perform their own check.
/// A later failure to acquire a particular child's pidfd still fails Prepare.
pub struct GuestWorkerReadiness {
    _private: (),
}

impl GuestWorkerReadiness {
    pub fn check() -> Result<Self, GuestReadinessError> {
        // SAFETY: getpid does not dereference memory or consume descriptors.
        let pid = unsafe { syscall1(nr::GETPID, 0) };
        loop {
            // SAFETY: pid identifies this live process; flags is zero.
            let descriptor = unsafe { syscall2(nr::PIDFD_OPEN, pid as usize, 0) };
            if descriptor == -(EINTR as isize) {
                continue;
            }
            if descriptor < 0 {
                return Err(GuestReadinessError::WorkerPidfdUnavailable {
                    errno: -descriptor as i32,
                });
            }
            // Linux releases the descriptor even when close reports EINTR.
            // Retrying could close another descriptor allocated in its place.
            close(descriptor as i32);
            return Ok(Self { _private: () });
        }
    }

    /// Selects the common profile, capabilities and limits before creating a worker.
    pub fn accept_hello(
        &self,
        protocol_version: ProtocolVersion,
        request: HelloRequest,
        broker: BrokerId,
        limits: ControlLimits,
    ) -> Result<GuestWorkerHello, GuestHelloError> {
        if protocol_version.major != request.offer().protocols.major
            || protocol_version.minor != lasper_guest_protocol::BOOTSTRAP_MANIFEST.minor
        {
            return Err(GuestHelloError::Negotiation(
                CompatibilityError::InvalidOffer,
            ));
        }
        let offer = self.offer(limits);
        let (response, negotiated) =
            HelloResponse::accept(request, offer, broker).map_err(GuestHelloError::Negotiation)?;
        Ok(GuestWorkerHello {
            response,
            negotiated,
        })
    }

    pub fn offer(&self, limits: ControlLimits) -> lasper_guest_protocol::HelloOffer {
        lasper_guest_protocol::HelloOffer::local(limits)
    }
}

/// The response and limits derived from one accepted host request.
pub struct GuestWorkerHello {
    response: HelloResponse,
    negotiated: lasper_guest_protocol::NegotiatedProtocol,
}

impl GuestWorkerHello {
    pub const fn negotiated(&self) -> lasper_guest_protocol::NegotiatedProtocol {
        self.negotiated
    }
    pub const fn response(&self) -> HelloResponse {
        self.response
    }

    pub const fn limits(&self) -> EnforcedControlLimits {
        self.negotiated.limits()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestHelloError {
    Negotiation(CompatibilityError),
}

impl fmt::Display for GuestHelloError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Negotiation(error) => {
                write!(formatter, "helper Hello negotiation failed: {error:?}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestReadinessError {
    WorkerPidfdUnavailable { errno: i32 },
}

impl fmt::Display for GuestReadinessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerPidfdUnavailable { errno } => write!(
                formatter,
                "helper cannot provide worker pidfds (errno {errno})"
            ),
        }
    }
}

#[cfg(test)]
#[path = "../tests/guardian/readiness.rs"]
mod tests;
