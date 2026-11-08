use crate::process::ProcessHandle;
use lasper_guest_protocol::{
    BrokerId, ControlLimits, EnforcedControlLimits, FeatureSet, HelloNegotiationError,
    HelloRequest, HelloResponse, PrepareReady, PrepareRequest, PrepareRequestFrame,
    MIN_HELPER_CONTROL_FRAME_BYTES,
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
        require_frame_capacity(limits.max_control_frame_bytes())?;
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
        let request = HelloRequest::new(FeatureSet::WORKER_PIDFD, limits).map_err(|error| {
            WorkerReadinessError::Negotiation(HelloNegotiationError::InvalidLimits(error))
        })?;
        Ok(Self { request })
    }

    pub const fn request(&self) -> HelloRequest {
        self.request
    }

    pub fn negotiate(
        self,
        protocol_major: u16,
        response: HelloResponse,
    ) -> Result<WorkerReadiness, WorkerReadinessError> {
        let limits = self
            .request
            .negotiate(protocol_major, response)
            .map_err(WorkerReadinessError::Negotiation)?;
        require_frame_capacity(limits.get().max_control_frame_bytes())?;
        Ok(WorkerReadiness {
            broker: response.broker_id(),
            limits,
        })
    }
}

/// Local inspection succeeded and this broker advertised worker assignment
/// by pidfd. Per-worker exit and access failures remain preparation errors.
pub struct WorkerReadiness {
    broker: BrokerId,
    limits: EnforcedControlLimits,
}

impl WorkerReadiness {
    pub const fn limits(&self) -> EnforcedControlLimits {
        self.limits
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
        let available = self.limits.get().max_descriptors();
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
    Negotiation(HelloNegotiationError),
    DescriptorLimit { required: u16, available: u16 },
    FrameLimit { available: u32 },
}

impl fmt::Display for WorkerReadinessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PidfdUnavailable(error) => write!(formatter, "helper execution requires host worker pidfd support: {error}"),
            Self::HostInspectionUnavailable(error) => write!(formatter, "helper execution requires host procfs inspection of worker pidfds: {error}"),
            Self::Negotiation(HelloNegotiationError::UnsupportedFeatures { required, available })
                if !available.contains(FeatureSet::WORKER_PIDFD) && required.contains(FeatureSet::WORKER_PIDFD) =>
                formatter.write_str("helper does not support worker pidfd assignment"),
            Self::Negotiation(error) => write!(formatter, "helper Hello negotiation failed: {error:?}"),
            Self::DescriptorLimit { required, available } => write!(formatter, "helper preparation requires {required} descriptors, negotiated limit is {available}"),
            Self::FrameLimit { available } => write!(formatter, "helper session needs at least {MIN_HELPER_CONTROL_FRAME_BYTES} control-frame bytes; available {available}"),
        }
    }
}

fn require_frame_capacity(available: u32) -> Result<(), WorkerReadinessError> {
    if available < MIN_HELPER_CONTROL_FRAME_BYTES {
        Err(WorkerReadinessError::FrameLimit { available })
    } else {
        Ok(())
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
