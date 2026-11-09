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
    NotPrepared,
    ReadyAlreadyTaken,
    AlreadyStarted,
    InvalidStart(lasper_guest_protocol::SessionPayloadError),
    InvalidStatus,
    Lifecycle(SessionLifecycleError),
}

pub(super) trait SystemResultExt<T> {
    fn context(self, operation: &'static str) -> Result<T, GuardianError>;
}

impl<T> SystemResultExt<T> for Result<T, i32> {
    fn context(self, operation: &'static str) -> Result<T, GuardianError> {
        self.map_err(|errno| GuardianError::System { operation, errno })
    }
}
