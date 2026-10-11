//! Frontend resource operations over application lifecycle services.

pub(crate) mod image_removal;
pub(crate) mod machine;

use crate::application::HostOperationTracker;
use crate::composition::PermissionManager;
use std::sync::Arc;

pub(crate) use image_removal::{ImageRemovalCompletion, PendingImageRemoval};
#[cfg(test)]
pub(crate) use machine::machine_action_can_change_x11_lifecycle;
pub(crate) use machine::machine_outcome_status;

pub(crate) struct ResourceActionServices {
    pub(crate) permissions: Arc<dyn PermissionManager>,
    pub(crate) host_operations: HostOperationTracker,
}
