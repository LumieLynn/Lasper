use super::DiskImageBackend;
use crate::nspawn::adapters::storage::ManagedImageKind;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::DiskImageSource;
use std::path::{Path, PathBuf};

impl DiskImageBackend {
    pub(super) async fn create_impl(&self, name: &str) -> Result<PathBuf> {
        if self.managed_kind() != Some(ManagedImageKind::Raw) {
            return Err(NspawnError::Validation(
                "Only new managed raw images can be created".into(),
            ));
        }

        match &self.config.source {
            DiskImageSource::CreateNew { size, fs_type } => {
                self.store
                    .create_raw_image(name, size, *fs_type, self.config.use_partition_table)
                    .await
            }
            DiskImageSource::ImportExisting { path } => {
                if path.is_empty() || path.trim() != path {
                    return Err(NspawnError::Validation(
                        "Source image path is required".into(),
                    ));
                }
                self.store.import_raw_image(name, Path::new(path)).await
            }
        }
    }
}
