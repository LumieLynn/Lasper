use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::{LegacyUnits, MARKER, PATH_UNIT, SERVICE_UNIT};
use crate::adapters::platform::x11::activation::test_support::FakeUnitManager;
use crate::adapters::trusted_state::TrustedDirectory;

#[tokio::test]
async fn migration_accepts_standard_unit_modes_and_preserves_external_files() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = temporary.path().metadata().unwrap().uid();
    let directory = TrustedDirectory::open_existing(temporary.path(), owner).unwrap();
    directory.write_atomic(PATH_UNIT, MARKER, 0o644).unwrap();
    directory
        .write_atomic(SERVICE_UNIT, b"[Service]\nExecStart=/bin/true\n", 0o644)
        .unwrap();
    let legacy = LegacyUnits::in_directory(directory).unwrap().unwrap();
    assert_eq!(legacy.owned, [PATH_UNIT]);
    legacy.remove(&FakeUnitManager::default()).await.unwrap();
    assert!(!temporary.path().join(PATH_UNIT).exists());
    assert_eq!(
        std::fs::read(temporary.path().join(SERVICE_UNIT)).unwrap(),
        b"[Service]\nExecStart=/bin/true\n"
    );
}
