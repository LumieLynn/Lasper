use std::path::Path;
use crate::nspawn::storage::{StorageType, StorageInfo};
use crate::nspawn::utils::{get_filesystem_type, new_sync_command};

pub fn detect_available_storage_types() -> StorageInfo {
    let machines_dir = Path::new("/var/lib/machines");
    let mut types = vec![
        (StorageType::Directory, true),
        (StorageType::DiskImage, true),
        (StorageType::Subvolume, false),
    ];

    if let Ok(fs_type) = get_filesystem_type(machines_dir) {
        if fs_type == "btrfs" || fs_type == "zfs" {
            for t in &mut types {
                if t.0 == StorageType::Subvolume {
                    t.1 = true;
                }
            }
        }
    }

    StorageInfo { types }
}

/// Check if a path is a Btrfs subvolume or ZFS dataset.
pub fn is_subvolume(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    if let Ok(fs_type) = get_filesystem_type(path) {
        if fs_type == "btrfs" {
            // Check if it's a subvolume using btrfs subvolume show
            // A subvolume has inode 256 as its root
            if let Ok(meta) = std::fs::metadata(path) {
                use std::os::unix::fs::MetadataExt;
                if meta.ino() == 256 {
                    return true;
                }
            }
            // Fallback to CLI if inode check is insufficient for some ragione
            let out = new_sync_command("btrfs")
                .args(["subvolume", "show", &path.to_string_lossy()])
                .output();
            return out.map(|o| o.status.success()).unwrap_or(false);
        } else if fs_type == "zfs" {
            // Check if it's a dataset
            let out = new_sync_command("zfs")
                .args(["list", &path.to_string_lossy()])
                .output();
            return out.map(|o| o.status.success()).unwrap_or(false);
        }
    }
    false
}
