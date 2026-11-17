use super::SessionLifecycleError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianError {
    System {
        operation: &'static str,
        errno: i32,
    },
    InvalidBootstrapDescriptors,
    StartupNotReleased,
    StartupExpired,
    HelloExpired,
    PrepareExpired,
    ControlExpired,
    ReplyEncoding,
    ReplyPending,
    PrepareFrame(lasper_guest_protocol::PrepareFrameError),
    SessionFrame(lasper_guest_protocol::SessionFrameError),
    PrepareStorage(super::PrepareStorageError),
    Hello(crate::readiness::GuestHelloError),
    HelloPayload(lasper_guest_protocol::PayloadCodecError),
    Readiness(crate::readiness::GuestReadinessError),
    Control(crate::transport::TransportError),
    BrokerDisconnected,
    UnexpectedBrokerData,
    SupervisorStopped(i32),
    SessionAlreadyCreated,
    MismatchedBroker {
        expected: lasper_guest_protocol::BrokerId,
        actual: lasper_guest_protocol::BrokerId,
    },
    ExistingChildren,
    ReadyDescriptorCapacity {
        required: u16,
        available: u16,
    },
    NotPrepared,
    ReadyAlreadyDelivered,
    AlreadyStarted,
    InvalidStart(lasper_guest_protocol::SessionPayloadError),
    InvalidStatus,
    Lifecycle(SessionLifecycleError),
}

impl GuardianError {
    pub fn class(self) -> crate::failure::FailureClass {
        use super::PrepareStorageError;
        use crate::failure::FailureClass;
        use crate::transport::TransportError;
        match self {
            Self::System { errno, .. }
            | Self::PrepareStorage(PrepareStorageError::System(errno)) => {
                FailureClass::system(errno)
            }
            Self::StartupExpired
            | Self::HelloExpired
            | Self::PrepareExpired
            | Self::ControlExpired => FailureClass::TimedOut,
            Self::Readiness(_) | Self::BrokerDisconnected | Self::SupervisorStopped(_) => {
                FailureClass::Unavailable
            }
            Self::Control(TransportError::Syscall(errno)) => FailureClass::system(errno),
            Self::Control(
                TransportError::UnexpectedEof
                | TransportError::WriteZero
                | TransportError::Poisoned,
            ) => FailureClass::Io,
            Self::Control(TransportError::ReceiveInProgress | TransportError::SendInProgress)
            | Self::InvalidBootstrapDescriptors
            | Self::ReplyEncoding
            | Self::ReplyPending
            | Self::SessionAlreadyCreated
            | Self::ExistingChildren
            | Self::NotPrepared
            | Self::ReadyAlreadyDelivered
            | Self::AlreadyStarted
            | Self::Lifecycle(_) => FailureClass::Internal,
            Self::StartupNotReleased
            | Self::PrepareFrame(_)
            | Self::SessionFrame(_)
            | Self::PrepareStorage(_)
            | Self::Hello(_)
            | Self::HelloPayload(_)
            | Self::Control(_)
            | Self::UnexpectedBrokerData
            | Self::MismatchedBroker { .. }
            | Self::ReadyDescriptorCapacity { .. }
            | Self::InvalidStart(_)
            | Self::InvalidStatus => FailureClass::Protocol,
        }
    }
}

pub(super) trait SystemResultExt<T> {
    fn context(self, operation: &'static str) -> Result<T, GuardianError>;
}

impl<T> SystemResultExt<T> for Result<T, i32> {
    fn context(self, operation: &'static str) -> Result<T, GuardianError> {
        self.map_err(|errno| GuardianError::System { operation, errno })
    }
}
