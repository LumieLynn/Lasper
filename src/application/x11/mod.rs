mod access;
mod acl;
mod catalog;
mod grants;
mod service;
mod session;

pub use access::{
    X11AccessError, X11Authorization, X11AuthorizationDisposition, X11DesktopAccessError,
    X11Revocation, X11RevocationDisposition,
};
pub(crate) use access::{
    X11AuthorizationPurpose, X11AuthorizationRequest, X11DesktopAccessPort,
    X11DesktopAuthorization, X11DesktopRevocation, X11ReconcileReport, X11RevokeRequest,
};
pub use acl::{X11AccessControlMode, X11AclEntry, X11AclSnapshot};
pub use catalog::{X11EndpointCatalog, X11SourceObservation, X11SourceState};
pub(crate) use catalog::{X11EndpointDiscoveryPort, X11EndpointDiscoveryService};
pub use grants::{
    X11AccessCheck, X11GrantAssessmentStatus, X11GrantHistoryStatus, X11MappedUidAclStatus,
};
pub(crate) use grants::{
    X11DesktopObservation, X11GrantRecordCatalog, X11GrantRecordEvidence, X11GrantRecordPhase,
};
#[allow(unused_imports)]
pub use grants::{X11GrantAssessment, X11GrantHistoryEntry};
pub use service::X11AccessService;
pub(crate) use service::X11ProjectionPort;
pub use session::{X11SessionPreparation, X11SessionSelection};

#[cfg(test)]
mod tests;
