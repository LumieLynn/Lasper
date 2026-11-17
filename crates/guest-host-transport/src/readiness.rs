use crate::process::ProcessHandle;
use crate::UnixControlStream;
use lasper_guest_protocol::{
    BrokerId, ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest, PayloadCodecError,
    PrepareReady, PrepareRequest, PrepareRequestFrame,
};
use std::fmt;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

/// Host checks performed before sending Hello. A guest capability bit cannot
/// establish that this host can inspect the process behind a received pidfd.
pub struct WorkerHello {
    request: HelloRequest,
}

impl WorkerHello {
    pub fn new(limits: ControlLimits) -> Result<Self, WorkerReadinessError> {
        Self::with_features(limits, FeatureSet::from_bits(0))
    }

    pub fn with_features(
        limits: ControlLimits,
        required: FeatureSet,
    ) -> Result<Self, WorkerReadinessError> {
        let minimum = lasper_guest_protocol::protocol_profiles()
            .iter()
            .map(|profile| profile.capacities().min_control_frame_bytes)
            .min()
            .expect("the build registers a protocol profile");
        if limits.max_control_frame_bytes() < minimum {
            return Err(WorkerReadinessError::FrameLimit {
                required: minimum,
                available: limits.max_control_frame_bytes(),
            });
        }
        let request = HelloRequest::from_offer(
            lasper_guest_protocol::HelloOffer::local(limits),
            FeatureSet::WORKER_PIDFD.union(required),
        )
        .map_err(WorkerReadinessError::InvalidRequest)?;
        // SAFETY: This opens a handle to the calling process, which remains
        // live throughout the local readiness check.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
        if descriptor < 0 {
            return Err(WorkerReadinessError::PidfdUnavailable(
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: pidfd_open returned a new descriptor with no other owner.
        let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
        ProcessHandle::open(pidfd).map_err(WorkerReadinessError::HostInspectionUnavailable)?;
        Ok(Self { request })
    }

    pub const fn request(&self) -> HelloRequest {
        self.request
    }

    /// Reads the completed Hello from this stream, which must remain usable.
    /// Checks that the stream negotiated this request. Profile capacity and
    /// feature requirements have already been enforced by the Hello exchange.
    pub fn complete(
        self,
        stream: &UnixControlStream,
    ) -> Result<WorkerReadiness, WorkerReadinessError> {
        let hello = stream
            .negotiated_hello()
            .ok_or(WorkerReadinessError::HelloUnavailable)?;
        if hello.request() != self.request {
            return Err(WorkerReadinessError::MismatchedHello);
        }
        Ok(WorkerReadiness {
            broker: hello.response().broker_id(),
            protocol: hello.protocol(),
        })
    }
}

/// Local inspection succeeded and this broker advertised worker assignment
/// by pidfd. Per-worker exit and access failures remain preparation errors.
pub struct WorkerReadiness {
    broker: BrokerId,
    protocol: lasper_guest_protocol::NegotiatedProtocol,
}

impl WorkerReadiness {
    pub const fn profile(&self) -> &'static lasper_guest_protocol::ProtocolProfile {
        self.protocol.profile()
    }
    pub const fn protocol(&self) -> lasper_guest_protocol::NegotiatedProtocol {
        self.protocol
    }
    pub const fn limits(&self) -> EnforcedControlLimits {
        self.protocol.limits()
    }

    pub const fn broker_id(&self) -> BrokerId {
        self.broker
    }

    pub fn prepare<'a>(
        &self,
        request_id: u64,
        request: PrepareRequest<'a>,
    ) -> Result<PrepareRequestFrame<'a>, WorkerReadinessError> {
        let required = PrepareReady::descriptor_count_for(request.execution());
        let available = self.limits().get().max_descriptors();
        if available < required {
            return Err(WorkerReadinessError::DescriptorLimit {
                required,
                available,
            });
        }
        Ok(PrepareRequestFrame::new(request_id, self.broker, request))
    }
}

#[derive(Debug)]
pub enum WorkerReadinessError {
    PidfdUnavailable(io::Error),
    HostInspectionUnavailable(io::Error),
    InvalidRequest(PayloadCodecError),
    HelloUnavailable,
    MismatchedHello,
    DescriptorLimit { required: u16, available: u16 },
    FrameLimit { required: u32, available: u32 },
}

impl fmt::Display for WorkerReadinessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PidfdUnavailable(error) => write!(formatter, "helper execution requires host worker pidfd support: {error}"),
            Self::HostInspectionUnavailable(error) => write!(formatter, "helper execution requires host procfs inspection of worker pidfds: {error}"),
            Self::InvalidRequest(error) => write!(formatter, "invalid local Hello request: {error:?}"),
            Self::HelloUnavailable => formatter.write_str("worker readiness requires a usable control stream with a completed Hello"),
            Self::MismatchedHello => formatter.write_str("completed Hello does not match this worker request"),
            Self::DescriptorLimit { required, available } => write!(formatter, "helper preparation requires {required} descriptors, negotiated limit is {available}"),
            Self::FrameLimit { required, available } => write!(formatter, "helper session needs at least {required} control-frame bytes; available {available}"),
        }
    }
}

impl std::error::Error for WorkerReadinessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PidfdUnavailable(error) | Self::HostInspectionUnavailable(error) => Some(error),
            _ => None,
        }
    }
}
