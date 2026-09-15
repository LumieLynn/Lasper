pub(crate) mod configuration;
pub mod nspawn_file;
pub(crate) mod nspawn_spec;
mod settings_lock;
pub mod store;
pub mod systemd_unit;

pub(crate) use nspawn_spec::{NspawnConfigSpec, ALL_DRM_DEVICES_PATH};
pub use store::NspawnConfigStore;
pub use systemd_unit::SystemdUnitStore;

const MAX_NSPAWN_CONTENT_BYTES: usize = 1024 * 1024;
