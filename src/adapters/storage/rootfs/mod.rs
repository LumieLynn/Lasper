//! Access to guest filesystem trees and explicitly owned raw-image mounts.

pub(crate) mod access;
mod error;
mod mount;
mod target;

pub(crate) use error::RootfsError;
pub(crate) use mount::RawRootfsMount;
pub(crate) use target::RootfsTarget;
