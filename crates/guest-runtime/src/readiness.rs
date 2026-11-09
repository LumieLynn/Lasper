use crate::linux::uapi::{errno::EINTR, nr};
use crate::socket::close;
use crate::syscall::{syscall1, syscall2};
use core::fmt;
use lasper_guest_protocol::{
    BrokerId, ControlLimits, EnforcedControlLimits, FeatureSet, HelloNegotiationError,
    HelloRequest, HelloResponse, ProtocolVersion, MIN_HELPER_CONTROL_FRAME_BYTES,
};

/// Checked in the broker's execution context before advertising worker pidfds.
/// A later failure to acquire a particular child's pidfd still fails Prepare.
pub struct GuestWorkerReadiness {
    _private: (),
}

impl GuestWorkerReadiness {
    pub fn check() -> Result<Self, GuestReadinessError> {
        // SAFETY: getpid does not dereference memory or consume descriptors.
        let pid = unsafe { syscall1(nr::GETPID, 0) };
        loop {
            // SAFETY: pid identifies this live broker; flags is zero.
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

    /// Accepts only hosts that explicitly request the worker-pidfd FD layout.
    /// Advertising our own support does not establish that an older host can
    /// receive it. No successful response is available before this check.
    pub fn accept_hello(
        &self,
        protocol_version: ProtocolVersion,
        request: HelloRequest,
        broker: BrokerId,
        limits: ControlLimits,
    ) -> Result<GuestWorkerHello, GuestHelloError> {
        let response =
            HelloResponse::new(FeatureSet::WORKER_PIDFD, broker, limits).map_err(|error| {
                GuestHelloError::Negotiation(HelloNegotiationError::InvalidLimits(error))
            })?;
        let limits = request
            .negotiate(protocol_version, response)
            .map_err(GuestHelloError::Negotiation)?;
        if !request
            .required_features()
            .contains(FeatureSet::WORKER_PIDFD)
        {
            return Err(GuestHelloError::WorkerPidfdNotRequested);
        }
        let available = limits.get().max_control_frame_bytes();
        if available < MIN_HELPER_CONTROL_FRAME_BYTES {
            return Err(GuestHelloError::FrameLimit { available });
        }
        Ok(GuestWorkerHello { response, limits })
    }
}

/// The response and limits derived from one accepted host request.
pub struct GuestWorkerHello {
    response: HelloResponse,
    limits: EnforcedControlLimits,
}

impl GuestWorkerHello {
    pub const fn response(&self) -> HelloResponse {
        self.response
    }

    pub const fn limits(&self) -> EnforcedControlLimits {
        self.limits
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestHelloError {
    WorkerPidfdNotRequested,
    FrameLimit { available: u32 },
    Negotiation(HelloNegotiationError),
}

impl fmt::Display for GuestHelloError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerPidfdNotRequested => formatter.write_str(
                "host did not request worker pidfd assignment; helper cannot use the legacy PrepareReady FD layout",
            ),
            Self::Negotiation(error) => write!(formatter, "helper Hello negotiation failed: {error:?}"),
            Self::FrameLimit { available } => write!(formatter, "helper session needs at least {MIN_HELPER_CONTROL_FRAME_BYTES} control-frame bytes; negotiated {available}"),
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
mod tests {
    use super::*;
    use lasper_guest_protocol::MAX_CONTROL_FRAME_BYTES;

    #[test]
    fn legacy_hosts_are_rejected_even_when_their_descriptor_limit_fits() {
        let readiness = GuestWorkerReadiness { _private: () };
        for descriptors in [2, 3] {
            let request = HelloRequest::new(
                FeatureSet::EMPTY,
                ControlLimits::new(MAX_CONTROL_FRAME_BYTES, descriptors).unwrap(),
            )
            .unwrap();
            assert!(matches!(
                readiness.accept_hello(
                    ProtocolVersion::CURRENT,
                    request,
                    BrokerId::new([1; 16]),
                    ControlLimits::CURRENT
                ),
                Err(GuestHelloError::WorkerPidfdNotRequested),
            ));
        }
    }

    #[test]
    fn accepted_worker_hello_keeps_negotiation_checks_and_limits() {
        let readiness = GuestWorkerReadiness { _private: () };
        let request = HelloRequest::new(
            FeatureSet::WORKER_PIDFD,
            ControlLimits::new(4096, 2).unwrap(),
        )
        .unwrap();
        let accepted = readiness
            .accept_hello(
                ProtocolVersion::CURRENT,
                request,
                BrokerId::new([1; 16]),
                ControlLimits::CURRENT,
            )
            .unwrap();
        assert_eq!(accepted.response().features(), FeatureSet::WORKER_PIDFD);
        assert_eq!(accepted.limits().get(), request.limits());
        assert_eq!(
            accepted.limits(),
            request
                .negotiate(ProtocolVersion::CURRENT, accepted.response())
                .unwrap()
        );
        assert!(matches!(
            readiness.accept_hello(
                ProtocolVersion {
                    major: ProtocolVersion::CURRENT.major + 1,
                    ..ProtocolVersion::CURRENT
                },
                request,
                BrokerId::new([1; 16]),
                ControlLimits::CURRENT
            ),
            Err(GuestHelloError::Negotiation(
                HelloNegotiationError::ProtocolVersion(_)
            )),
        ));
        let unsupported = HelloRequest::new(
            FeatureSet::from_bits(FeatureSet::WORKER_PIDFD.bits() | (1 << 8)),
            ControlLimits::CURRENT,
        )
        .unwrap();
        assert!(matches!(
            readiness.accept_hello(
                ProtocolVersion::CURRENT,
                unsupported,
                BrokerId::new([1; 16]),
                ControlLimits::CURRENT
            ),
            Err(GuestHelloError::Negotiation(
                HelloNegotiationError::UnsupportedFeatures { .. }
            )),
        ));
    }
}
