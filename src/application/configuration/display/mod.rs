//! Persistent display-integration configuration contracts.
//!
//! This module groups the shared bind policy with the X11 and Wayland
//! declarations it governs. Runtime projection and access control remain in
//! their respective application services.

use serde::{Deserialize, Serialize};

mod wayland;
mod x11;

pub use wayland::{
    recommended_wayland_target, WaylandBindRecommendation, WaylandBindingChange,
    WaylandBindingDeclaration, WaylandEndpointCatalog, WaylandSourceObservation,
    WaylandSourceState,
};
pub(crate) use wayland::{WaylandEndpointDiscoveryPort, WaylandEndpointDiscoveryService};
pub use x11::{X11BindRecommendation, X11BindingChange, X11BindingDeclaration, X11BindingScope};

/// Policy for a newly selected host display endpoint. This is derived from
/// the inspected startup configuration; callers cannot choose the mount
/// suffix independently of `PrivateUsers=`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DisplayBindRecommendation {
    Ready {
        private_users: String,
        idmapped: bool,
    },
    Unsupported {
        private_users: String,
        reason: String,
    },
}
