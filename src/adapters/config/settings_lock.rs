//! One lock namespace for all Lasper writers of machine-keyed settings.
//! Neither atomic replacement nor removal of a .nspawn/sidecar file releases
//! another writer's lock. Read-only inspection never takes this lock.

use std::fs::File;

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::trusted_state::TrustedDirectory;
use crate::domain::machine::MachineName;

pub(super) async fn acquire(machine: MachineName) -> Result<File> {
    tokio::task::spawn_blocking(move || {
        let directory =
            TrustedDirectory::open_or_create(&crate::paths::nspawn_settings_locks_dir(), 0)?;
        directory.lock_exclusive(machine.as_str())
    })
    .await
    .map_err(|error| NspawnError::Runtime(format!("configuration lock failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::os::unix::fs::{symlink, MetadataExt};

    #[test]
    fn machine_lock_survives_configuration_and_sidecar_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let directory = TrustedDirectory::for_test(&temp.path().join("locks")).unwrap();
        let first = directory.lock_exclusive("archlinux").unwrap();
        let lock_path = temp.path().join("locks/.archlinux.lock");
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        let settings = temp.path().join("archlinux.nspawn");
        std::fs::write(&settings, "[Files]\n").unwrap();
        std::fs::write(crate::adapters::filesystem::lock_path_for(&settings), "").unwrap();
        std::fs::remove_file(&settings).unwrap();
        std::fs::remove_file(crate::adapters::filesystem::lock_path_for(&settings)).unwrap();
        std::fs::write(&settings, "[Files]\nBind=/dev/dri\n").unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        let other = directory.lock_exclusive("other-machine").unwrap();
        drop(other);
        drop(first);
        contender.try_lock_exclusive().unwrap();
        let inode = contender.metadata().unwrap().ino();
        drop(contender);
        let reopened = TrustedDirectory::for_test(&temp.path().join("locks"))
            .unwrap()
            .lock_exclusive("archlinux")
            .unwrap();
        assert_eq!(reopened.metadata().unwrap().ino(), inode);
    }

    #[test]
    fn lock_file_and_directory_symlinks_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = TrustedDirectory::for_test(&temp.path().join("locks")).unwrap();
        let external = temp.path().join("external");
        std::fs::write(&external, "untouched").unwrap();
        symlink(&external, temp.path().join("locks/.archlinux.lock")).unwrap();
        assert!(directory.lock_exclusive("archlinux").is_err());
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "untouched");
        symlink(temp.path().join("locks"), temp.path().join("alias")).unwrap();
        assert!(TrustedDirectory::for_test(&temp.path().join("alias")).is_err());
    }
}
