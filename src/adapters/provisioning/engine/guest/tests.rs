use super::*;
use crate::adapters::process::{MockCommandRunner, MockRootfsProcessRunner};
use crate::domain::machine::MachineName;

#[tokio::test]
async fn mutation_values_are_validated_before_rootfs_state() {
    let guest = GuestInitializer {
        commands: Arc::new(MockCommandRunner::new()),
        processes: Arc::new(MockRootfsProcessRunner::new()),
    };
    let result = guest
        .set_root_password(
            &RootfsTarget::Machine {
                machine: MachineName::new("missing-test-machine").unwrap(),
            },
            SecretString::new("bad\npassword".into()),
        )
        .await;
    assert!(matches!(result, Err(OperationFailure::InvalidInput(_))));
}

#[test]
fn rootfs_probe_resolves_absolute_symlinks_inside_the_rootfs() {
    let rootfs = tempfile::tempdir().unwrap();
    let etc = rootfs.path().join("etc");
    let static_etc = rootfs.path().join("etc/static");
    std::fs::create_dir_all(&static_etc).unwrap();
    std::fs::write(static_etc.join("os-release"), "NAME=NixOS\n").unwrap();
    std::os::unix::fs::symlink("/etc/static/os-release", etc.join("os-release")).unwrap();
    assert!(path_exists_in_root(rootfs.path(), "etc/os-release").unwrap());
}

#[test]
fn rootfs_probe_does_not_follow_absolute_symlinks_onto_the_host() {
    let rootfs = tempfile::tempdir().unwrap();
    let etc = rootfs.path().join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", etc.join("os-release")).unwrap();
    assert!(!path_exists_in_root(rootfs.path(), "etc/os-release").unwrap());
}

#[test]
fn nspawn_command_probe_requires_usr_inside_rootfs() {
    let rootfs = tempfile::tempdir().unwrap();
    assert!(!path_is_directory_in_root(rootfs.path(), "usr").unwrap());
    std::fs::write(rootfs.path().join("usr"), "not a directory").unwrap();
    assert!(!path_is_directory_in_root(rootfs.path(), "usr").unwrap());
    std::fs::remove_file(rootfs.path().join("usr")).unwrap();
    std::fs::create_dir(rootfs.path().join("usr")).unwrap();
    assert!(path_is_directory_in_root(rootfs.path(), "usr").unwrap());
}
