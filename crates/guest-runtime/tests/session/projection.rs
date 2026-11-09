extern crate std;
use super::*;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;

#[test]
fn socket_identity_matches_stat_and_a_non_socket_is_distinct_from_absence() {
    let path = std::env::temp_dir().join(std::format!("lasper-projection-{}", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let observation = observe_path(path.to_str().unwrap()).unwrap();
    assert_eq!(observation.access(), SocketPathAccess::Accessible);
    assert_eq!(
        observation.identity(),
        Some(SocketPathIdentity::new(metadata.dev(), metadata.ino()))
    );
    drop(listener);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        observe_path(path.to_str().unwrap()).unwrap().access(),
        SocketPathAccess::Unavailable
    );
    std::fs::write(&path, b"file").unwrap();
    assert_eq!(
        observe_path(path.to_str().unwrap()).unwrap().access(),
        SocketPathAccess::NotSocket
    );
    std::fs::remove_file(path).unwrap();
}
