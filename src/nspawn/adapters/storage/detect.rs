use crate::nspawn::adapters::storage::{StorageInfo, StorageType};
use crate::nspawn::sys::get_filesystem_type;
use std::path::Path;

pub async fn detect_available_storage_types() -> StorageInfo {
    let machines_dir = crate::paths::machines_dir();
    let mut types = vec![
        (StorageType::Directory, true),
        (StorageType::DiskImage, true),
        (StorageType::Subvolume, false),
    ];

    if let Ok(fs_type) = get_filesystem_type(&machines_dir).await {
        if fs_type == "btrfs" {
            for t in &mut types {
                if t.0 == StorageType::Subvolume {
                    t.1 = true;
                }
            }
        }
    }

    StorageInfo { types }
}

/// Check if a path is a Btrfs subvolume.
#[allow(dead_code)]
pub async fn is_subvolume(path: &Path) -> bool {
    crate::nspawn::adapters::storage::subvolume_ops::is_subvolume(path).await
}
