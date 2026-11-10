use lasper_rootfs_path::RootfsAccess;
use std::fs;
use std::io;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};

#[test]
fn absolute_and_parent_links_stay_inside_the_guest_root() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("etc")).unwrap();
    fs::create_dir_all(root.path().join("store")).unwrap();
    fs::write(root.path().join("store/passwd"), "guest data").unwrap();
    symlink("/store/passwd", root.path().join("etc/passwd")).unwrap();
    let access = RootfsAccess::open(root.path()).unwrap();
    let entry = access.entry("etc/passwd").unwrap().unwrap();
    assert_eq!(
        entry.metadata().unwrap().ino(),
        fs::metadata(root.path().join("store/passwd"))
            .unwrap()
            .ino(),
    );

    fs::remove_file(root.path().join("etc/passwd")).unwrap();
    symlink("../../../../etc/shadow", root.path().join("etc/passwd")).unwrap();
    assert!(access.entry("etc/passwd").unwrap().is_none());
}

#[test]
fn an_opened_root_and_entry_survive_path_replacement() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), "original").unwrap();
    let original = fs::metadata(root.join("etc/passwd")).unwrap();
    let access = RootfsAccess::open(&root).unwrap();
    fs::rename(&root, parent.path().join("old")).unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), "replacement root").unwrap();
    let file = access.entry("etc/passwd").unwrap().unwrap();
    fs::remove_file(parent.path().join("old/etc/passwd")).unwrap();
    fs::write(parent.path().join("old/etc/passwd"), "replacement file").unwrap();
    let pinned = file.metadata().unwrap();
    assert_eq!(
        (pinned.dev(), pinned.ino()),
        (original.dev(), original.ino())
    );
    assert_eq!(pinned.len(), 8);
}

#[test]
fn entries_can_pin_directories_and_fifos_without_opening_them_for_io() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("directory")).unwrap();
    let fifo =
        std::ffi::CString::new(root.path().join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fifo is a NUL-terminated path inside this test's temporary root.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let access = RootfsAccess::open(root.path()).unwrap();
    let directory = access.entry("directory").unwrap().unwrap();
    let fifo = access.entry("fifo").unwrap().unwrap();
    assert!(directory.metadata().unwrap().is_dir());
    assert!(fifo.metadata().unwrap().file_type().is_fifo());
}

#[test]
fn a_missing_root_is_an_error_rather_than_a_missing_guest_file() {
    let parent = tempfile::tempdir().unwrap();
    let error = RootfsAccess::open(&parent.path().join("missing"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn socket_inode_remains_the_connection_target_after_path_replacement() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("private")).unwrap();
    symlink("/private", root.path().join("run")).unwrap();
    let path = root.path().join("private/broker.sock");
    let original = UnixListener::bind(&path).unwrap();
    original.set_nonblocking(true).unwrap();
    let access = RootfsAccess::open(root.path()).unwrap();
    let socket = access.entry("/run/broker.sock").unwrap().unwrap();
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    replacement.set_nonblocking(true).unwrap();

    let _client = UnixStream::connect(format!("/proc/self/fd/{}", socket.as_raw_fd())).unwrap();
    let _accepted = original.accept().unwrap();
    assert_eq!(
        replacement.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn proc_magic_links_are_not_an_escape_from_the_guest_root() {
    let access = RootfsAccess::open(std::path::Path::new("/")).unwrap();
    assert!(access.entry("/proc/self/root/etc/passwd").is_err());
}
