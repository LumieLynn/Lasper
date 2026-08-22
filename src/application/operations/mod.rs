//! Coordination and route semantics shared by application workflows.

mod activity;
pub mod registry;
pub mod route;

pub use activity::HostOperationTracker;
pub use registry::{
    OperationRegistry, ResourceClaim, ResourceConflict, ResourceKey, ResourceReservation,
};
pub use route::{ExecutionRoute, RouteFallback};
