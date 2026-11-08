extern crate std;

use std::io::{Error, ErrorKind};

use super::open;

#[test]
fn directory_open_accepts_directories_and_rejects_regular_files() {
    assert!(open(c"/proc/self", true).is_ok());
    assert!(open(c"/proc/self/status", false).is_ok());
    let errno = open(c"/proc/self/status", true)
        .err()
        .expect("opening a regular file as a directory must fail");
    assert_eq!(
        Error::from_raw_os_error(errno).kind(),
        ErrorKind::NotADirectory
    );
}
