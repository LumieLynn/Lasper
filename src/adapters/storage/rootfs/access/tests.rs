use super::RootfsAccess;
use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;

#[test]
fn regular_file_reads_guest_absolute_links_without_escaping_the_root() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("etc")).unwrap();
    fs::create_dir(root.path().join("store")).unwrap();
    fs::write(root.path().join("store/passwd"), "guest data").unwrap();
    symlink("/store/passwd", root.path().join("etc/passwd")).unwrap();
    let access = RootfsAccess::open(root.path()).unwrap();
    let mut content = String::new();
    access
        .regular_file("etc/passwd")
        .unwrap()
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "guest data");

    fs::remove_file(root.path().join("etc/passwd")).unwrap();
    symlink("../../../../etc/shadow", root.path().join("etc/passwd")).unwrap();
    assert!(access.regular_file("etc/passwd").unwrap().is_none());
}

#[test]
fn non_regular_files_are_rejected_without_opening_them_for_io() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("directory")).unwrap();
    let fifo =
        std::ffi::CString::new(root.path().join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fifo names an entry in this test's private temporary directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let _listener = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    let access = RootfsAccess::open(root.path()).unwrap();
    for path in ["directory", "fifo", "socket"] {
        assert_eq!(
            access.regular_file(path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn file_reads_from_a_pinned_root_survive_root_and_path_replacement() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), "original").unwrap();
    let access = RootfsAccess::from_directory(fs::File::open(&root).unwrap()).unwrap();
    fs::rename(&root, parent.path().join("old")).unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), "replacement root").unwrap();
    let mut file = access.regular_file("etc/passwd").unwrap().unwrap();
    fs::remove_file(parent.path().join("old/etc/passwd")).unwrap();
    fs::write(parent.path().join("old/etc/passwd"), "replacement file").unwrap();
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "original");
    // SAFETY: F_GETFD and F_GETFL only inspect this still-owned file descriptor.
    let (descriptor_flags, status_flags) = unsafe {
        (
            libc::fcntl(file.as_raw_fd(), libc::F_GETFD),
            libc::fcntl(file.as_raw_fd(), libc::F_GETFL),
        )
    };
    assert!(descriptor_flags >= 0 && descriptor_flags & libc::FD_CLOEXEC != 0);
    assert!(status_flags >= 0 && status_flags & libc::O_NONBLOCK != 0);
}

#[test]
fn a_missing_root_is_distinct_from_a_missing_guest_file() {
    let parent = tempfile::tempdir().unwrap();
    let error = RootfsAccess::open(&parent.path().join("missing"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let access = RootfsAccess::open(parent.path()).unwrap();
    assert!(access.regular_file("missing").unwrap().is_none());
}
