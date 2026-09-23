//! Bounded constants shared by the private X11 platform modules.
//!
//! This module intentionally exports no types or functions.  Each X11
//! responsibility declares its own standard-library, x11rb, application, and
//! domain dependencies so that the module boundaries remain visible at the
//! import site.

pub(super) const X11_SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";
pub(super) const ENDPOINT_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
pub(super) const MAX_DISCOVERED_DISPLAYS: usize = 16;
pub(super) const MAX_INSPECTED_SOURCES: usize = 64;
pub(super) const MAX_ACL_ENTRIES: usize = 1024;
pub(super) const MAX_ACL_BYTES: usize = 64 * 1024;
pub(super) const GRANT_RECORD_VERSION: u32 = 1;
pub(super) const MAX_GRANT_RECORDS: usize = 256;
pub(super) const MAX_GRANT_RECORD_BYTES: usize = 16 * 1024;
pub(super) const MAX_GRANT_DIAGNOSTICS: usize = 16;
pub(super) const MAX_GRANT_REASON_BYTES: usize = 2048;
pub(super) const MACHINE_CLAIM_VERSION: u32 = 1;
pub(super) const MAX_MACHINE_CLAIMS: usize = 256;
pub(super) const MAX_MACHINE_CLAIM_BYTES: usize = 16 * 1024;
pub(super) const CLAIM_RECONCILE_GRACE_MILLIS: u64 = 2_000;
