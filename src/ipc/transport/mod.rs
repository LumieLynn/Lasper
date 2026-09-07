//! Unix socket creation, peer authorization, and bounded frame transport.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

pub(crate) const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;

pub(crate) fn create_fd_socket_dir(user_uid: u32) -> std::io::Result<tempfile::TempDir> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if is_private_writable_runtime_dir(&path, user_uid) {
            candidates.push(path);
        }
    }
    let system_runtime = PathBuf::from(format!("/run/user/{user_uid}"));
    if !candidates.contains(&system_runtime)
        && is_private_writable_runtime_dir(&system_runtime, user_uid)
    {
        candidates.push(system_runtime);
    }

    create_fd_socket_dir_from_candidates(user_uid, &candidates)
}

pub(crate) fn create_fd_socket_dir_from_candidates(
    user_uid: u32,
    candidates: &[PathBuf],
) -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::MetadataExt;

    let mut directory = None;
    for path in candidates {
        match create_private_tempdir(Some(path)) {
            Ok(candidate) => {
                directory = Some(candidate);
                break;
            }
            Err(error) if runtime_dir_error_allows_fallback(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    let directory = match directory {
        Some(directory) => directory,
        None => create_private_tempdir(None)?,
    };

    let metadata = std::fs::symlink_metadata(directory.path())?;
    if metadata.uid() != user_uid || metadata.mode() & 0o777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "fd socket directory ownership or mode verification failed: \
                 path={}, uid={} (expected {}), mode={:o} (expected 700)",
                directory.path().display(),
                metadata.uid(),
                user_uid,
                metadata.mode() & 0o777
            ),
        ));
    }
    Ok(directory)
}

fn create_private_tempdir(parent: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;

    let mut builder = tempfile::Builder::new();
    builder.prefix("lasper-");
    builder.permissions(std::fs::Permissions::from_mode(0o700));
    match parent {
        Some(path) => builder.tempdir_in(path),
        None => builder.tempdir(),
    }
}

pub(crate) fn is_private_writable_runtime_dir(path: &Path, user_uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == user_uid
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o300 == 0o300
    })
}

pub(crate) fn runtime_dir_error_allows_fallback(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotFound
    )
}

pub(crate) fn configure_user_socket(path: &Path, user_uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let path_bytes = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon socket path contains a NUL byte",
        )
    })?;
    let raw_fd = unsafe {
        libc::open(
            path_bytes.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let before = descriptor_metadata(&socket)?;
    if before.mode() & libc::S_IFMT != libc::S_IFSOCK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon socket path is not a Unix socket",
        ));
    }

    if before.uid() != user_uid {
        let result = unsafe {
            libc::fchownat(
                socket.as_raw_fd(),
                c"".as_ptr(),
                user_uid,
                libc::gid_t::MAX,
                libc::AT_EMPTY_PATH,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    chmod_descriptor(&socket, 0o600)?;

    let secured = descriptor_metadata(&socket)?;
    let current = std::fs::symlink_metadata(path)?;
    if secured.uid() != user_uid
        || secured.mode() & 0o777 != 0o600
        || secured.dev() != current.dev()
        || secured.ino() != current.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon socket identity, ownership, or mode verification failed",
        ));
    }
    Ok(())
}

fn descriptor_metadata(fd: &OwnedFd) -> std::io::Result<std::fs::Metadata> {
    std::fs::File::from(fd.try_clone()?).metadata()
}

fn chmod_descriptor(fd: &OwnedFd, mode: libc::mode_t) -> std::io::Result<()> {
    let result = unsafe { libc::fchmodat(fd.as_raw_fd(), c"".as_ptr(), mode, libc::AT_EMPTY_PATH) };
    if result == 0 {
        return Ok(());
    }

    let mut error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINVAL) {
        let result = unsafe {
            libc::syscall(
                libc::SYS_fchmodat2,
                fd.as_raw_fd(),
                c"".as_ptr(),
                mode,
                libc::AT_EMPTY_PATH,
            )
        };
        if result == 0 {
            return Ok(());
        }
        error = std::io::Error::last_os_error();
    }
    if !matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) {
        return Err(error);
    }

    // Linux kernels predating fchmodat2 can still address the pinned O_PATH
    // inode through procfs. The path contains only the already-open fd number.
    let proc_path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .expect("fd path cannot contain NUL");
    let result = unsafe { libc::chmod(proc_path.as_ptr(), mode) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Read one protocol frame without allowing a peer to grow an unbounded line
/// in memory. Callers discard the connection after an oversized frame.
pub(crate) async fn read_bounded_line<R>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut limited = reader.take((limit as u64).saturating_add(1));
    let mut bytes = Vec::new();
    let count = limited.read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("protocol frame exceeds {limit} bytes"),
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FdAuthorizationError {
    UnexpectedUid { actual: u32, expected: u32 },
    UnexpectedPid { actual: u32, expected: u32 },
    InvalidToken,
}

/// Read kernel-supplied credentials for the peer of a Unix socket.
pub(crate) fn get_peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    let fd = stream.as_raw_fd();
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pid = u32::try_from(ucred.pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "peer reported an invalid PID",
        )
    })?;
    Ok(PeerCredentials {
        pid,
        uid: ucred.uid,
    })
}

pub(crate) fn authorize_fd_peer(
    actual: PeerCredentials,
    expected: PeerCredentials,
) -> Result<(), FdAuthorizationError> {
    if actual.uid != expected.uid {
        return Err(FdAuthorizationError::UnexpectedUid {
            actual: actual.uid,
            expected: expected.uid,
        });
    }
    if actual.pid != expected.pid {
        return Err(FdAuthorizationError::UnexpectedPid {
            actual: actual.pid,
            expected: expected.pid,
        });
    }
    Ok(())
}

pub(crate) fn authorize_root_server(actual: PeerCredentials) -> std::io::Result<()> {
    if actual.uid == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "elevated daemon socket peer has uid {} instead of root (pid {})",
                actual.uid, actual.pid
            ),
        ))
    }
}

pub(crate) fn authorize_fd_token(actual: &str, expected: &str) -> Result<(), FdAuthorizationError> {
    if actual.len() != expected.len() {
        return Err(FdAuthorizationError::InvalidToken);
    }

    let difference = actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0u8, |difference, (actual, expected)| {
            difference | (actual ^ expected)
        });
    if difference == 0 {
        Ok(())
    } else {
        Err(FdAuthorizationError::InvalidToken)
    }
}
