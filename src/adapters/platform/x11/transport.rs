//! X11 wire transport and endpoint revalidation primitives.
//!
//! This module owns Unix-socket connections, Xauthority fallback, ACL wire
//! encoding, peer credentials, and replacement/generation checks. Higher
//! modules decide when a transport operation is allowed.

use std::fs;
use std::io::{self, IoSlice};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use x11rb::protocol::xproto::{ConnectionExt, Family, HostMode};
use x11rb::reexports::x11rb_protocol::xauth;
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use crate::application::x11::{X11AclEntry, X11AclSnapshot};
use crate::domain::x11::{HostX11Socket, X11PeerIdentity, X11SocketRevision};

use super::common::{ENDPOINT_IO_TIMEOUT, MAX_ACL_BYTES, MAX_ACL_ENTRIES};

type X11Connection = RustConnection<EndpointStream>;
type AuthenticatedX11Connection = (X11Connection, X11PeerIdentity);

/// x11rb makes its stream nonblocking and waits through `Stream::poll`, so
/// socket read/write timeouts do not limit setup or reply waits. Keep its
/// wire/FD handling and give each setup/query/change an absolute deadline.
/// Partial replies and repeated readiness notifications must not extend it.
#[derive(Debug)]
pub(super) struct EndpointStream {
    inner: DefaultStream,
    deadline: Mutex<Instant>,
}

impl EndpointStream {
    fn begin_operation(&self) {
        *self.deadline.lock() = Instant::now() + ENDPOINT_IO_TIMEOUT;
    }

    fn check_deadline(&self) -> io::Result<()> {
        if Instant::now() >= *self.deadline.lock() {
            Err(endpoint_timeout())
        } else {
            Ok(())
        }
    }
}

impl Stream for EndpointStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut events = 0;
        if mode.readable() {
            events |= libc::POLLIN;
        }
        if mode.writable() {
            events |= libc::POLLOUT;
        }
        wait_for_socket(self.inner.as_raw_fd(), events, *self.deadline.lock())
    }

    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.read(buf, fds)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.write(buf, fds)
    }

    fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.write_vectored(bufs, fds)
    }
}

fn endpoint_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "X11 endpoint wait timed out")
}

fn wait_for_socket(fd: RawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(endpoint_timeout());
        }
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        // The descriptor remains owned by the caller throughout this wait.
        let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if result > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            // HUP/ERR are handled by the next stream read/write.
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn sockaddr_for(source: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let path = source.as_os_str().as_bytes();
    // Zero initialization supplies the terminating NUL and sockaddr padding.
    let mut sockaddr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.is_empty() || path.len() >= sockaddr.sun_path.len() || path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid X11 socket path",
        ));
    }
    sockaddr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in sockaddr.sun_path.iter_mut().zip(path) {
        *destination = *source as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1) as libc::socklen_t;
    Ok((sockaddr, length))
}

fn connect_socket(source: &Path, deadline: Instant) -> io::Result<UnixStream> {
    const BACKLOG_RETRY_DELAY: Duration = Duration::from_millis(10);

    let (address, length) = sockaddr_for(source)?;
    // Nonblocking connect also avoids hanging on a full Unix socket backlog.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // socket() returned a new descriptor; OwnedFd closes every error path.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 socket connection timed out",
            ));
        }
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                std::ptr::addr_of!(address).cast(),
                length,
            )
        };
        if result == 0 {
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                // Linux AF_UNIX returns EAGAIN when the listener backlog is
                // full, without starting a connection. Retry connect rather
                // than wait for TCP-style asynchronous completion.
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(BACKLOG_RETRY_DELAY),
                );
            }
            _ => return Err(error),
        }
    }
}

pub(super) fn read_acl(connection: &X11Connection, display: u16) -> Result<X11AclSnapshot, String> {
    connection.stream().begin_operation();
    let reply = connection
        .list_hosts()
        .map_err(|error| format!("send ListHosts to :{display}: {error}"))?
        .reply()
        .map_err(|error| format!("read ListHosts from :{display}: {error}"))?;

    if reply.hosts.len() > MAX_ACL_ENTRIES {
        return Err(format!(
            "X server returned {} ACL entries; the safety limit is {MAX_ACL_ENTRIES}",
            reply.hosts.len()
        ));
    }
    let mut total_bytes = 0usize;
    let mut entries = Vec::with_capacity(reply.hosts.len());
    for host in reply.hosts {
        total_bytes = total_bytes
            .checked_add(host.address.len())
            .ok_or("X11 ACL byte count overflowed")?;
        if total_bytes > MAX_ACL_BYTES {
            return Err(format!(
                "X server returned more than {MAX_ACL_BYTES} bytes of ACL entries"
            ));
        }
        entries.push(X11AclEntry::from_wire(host.family.into(), host.address));
    }
    Ok(X11AclSnapshot::from_wire(reply.mode.into(), entries))
}
pub(super) fn insert_and_observe(
    connection: &X11Connection,
    display: u16,
    host_uid: u32,
) -> (Result<(), String>, Result<X11AclSnapshot, String>) {
    connection.stream().begin_operation();
    let address = numeric_local_user_address(host_uid);
    let change_result =
        match connection.change_hosts(HostMode::INSERT, Family::SERVER_INTERPRETED, &address) {
            Ok(cookie) => cookie
                .check()
                .map_err(|error| format!("insert localuser:#{host_uid}: {error}")),
            Err(error) => Err(format!("send localuser:#{host_uid} insertion: {error}")),
        };
    let observed = read_acl(connection, display);
    (change_result, observed)
}

pub(super) fn remove_and_observe(
    connection: &X11Connection,
    display: u16,
    host_uid: u32,
) -> (Result<(), String>, Result<X11AclSnapshot, String>) {
    connection.stream().begin_operation();
    let address = numeric_local_user_address(host_uid);
    let change_result =
        match connection.change_hosts(HostMode::DELETE, Family::SERVER_INTERPRETED, &address) {
            Ok(cookie) => cookie
                .check()
                .map_err(|error| format!("remove localuser:#{host_uid}: {error}")),
            Err(error) => Err(format!("send localuser:#{host_uid} removal: {error}")),
        };
    let observed = read_acl(connection, display);
    (change_result, observed)
}

pub(super) fn authorization_was_confirmed(
    change: &Result<(), String>,
    observation: &Result<X11AclSnapshot, String>,
    socket: &Result<(), String>,
    server: &Result<(), String>,
    host_uid: u32,
) -> bool {
    change.is_ok()
        && socket.is_ok()
        && server.is_ok()
        && observation
            .as_ref()
            .is_ok_and(|snapshot| snapshot.has_numeric_local_user(host_uid))
}

pub(super) fn numeric_local_user_address(uid: u32) -> Vec<u8> {
    format!("localuser\0#{uid}").into_bytes()
}

pub(super) fn user_runtime_directory(uid: u32) -> Result<PathBuf, String> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    if !path.is_absolute() {
        return Err(format!(
            "XDG_RUNTIME_DIR must be absolute: {}",
            path.display()
        ));
    }
    Ok(path)
}

pub(super) fn host_boot_id() -> Result<String, String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read host boot identity: {error}"))?;
    uuid::Uuid::parse_str(value.trim())
        .map(|value| value.to_string())
        .map_err(|error| format!("parse host boot identity: {error}"))
}

pub(super) fn unix_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system time: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "system time exceeds the X11 record format".into())
}

pub(super) fn require_current_socket(socket: &HostX11Socket, phase: &str) -> Result<(), String> {
    let canonical = fs::canonicalize(socket.source())
        .map_err(|error| format!("resolve {} {phase}: {error}", socket.source().display()))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|error| format!("inspect {} {phase}: {error}", canonical.display()))?;
    if !socket_metadata_matches(socket, &canonical, &metadata) {
        return Err(format!("{} changed {phase}", socket.source().display()));
    }
    Ok(())
}

fn socket_metadata_matches(
    socket: &HostX11Socket,
    canonical: &Path,
    metadata: &fs::Metadata,
) -> bool {
    let revision = X11SocketRevision {
        device: metadata.dev(),
        inode: metadata.ino(),
        ctime_seconds: metadata.ctime(),
        ctime_nanoseconds: metadata.ctime_nsec(),
    };
    metadata.file_type().is_socket()
        && canonical == socket.canonical_path()
        && revision == socket.revision()
        && metadata.uid() == socket.owner_uid()
        && metadata.gid() == socket.owner_gid()
        // HostX11Socket stores permission bits, not the file type in st_mode.
        && (metadata.permissions().mode() & 0o7777) == socket.mode()
}

/// Bind ACL operations to the actual connection's peer. Later filesystem
/// checks detect pathname replacement without making throwaway connections;
/// they do not attempt to establish a different peer's identity.
pub(super) fn connect_current_socket(socket: &HostX11Socket) -> Result<X11Connection, String> {
    require_current_socket(socket, "before connecting")?;
    let (connection, peer) = authenticated_connection(socket.source(), socket.display())?;
    if peer != socket.peer_identity() {
        return Err(format!(
            "{} peer changed while connecting",
            socket.source().display()
        ));
    }
    require_current_socket(socket, "while connecting")?;
    Ok(connection)
}

pub(super) fn authenticated_connection(
    source: &Path,
    display: u16,
) -> Result<AuthenticatedX11Connection, String> {
    let deadline = Instant::now() + ENDPOINT_IO_TIMEOUT;
    let stream = connect_socket(source, deadline)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    let peer = peer_credentials(&stream)?;
    let (stream, (family, peer_address)) = DefaultStream::from_unix_stream(stream)
        .map_err(|error| format!("prepare X11 connection: {error}"))?;
    let (auth_name, auth_data) = xauth::get_auth(family, &peer_address, display)
        .ok()
        .flatten()
        .unwrap_or_default();
    let has_authority = !auth_name.is_empty() || !auth_data.is_empty();

    // Authority lookup describes this local transport; setup must use the
    // same stream rather than discard a connection just to obtain its address.
    match setup_x11(source, stream, auth_name, auth_data, deadline) {
        Ok(connection) => Ok((connection, peer)),
        Err(with_authority_error) if has_authority => {
            // Some Xwayland environments expose a valid pathname socket but
            // deliberately run without Xauthority. A stale local cookie must
            // not hide that endpoint; retrying with empty credentials merely
            // lets the X server apply its own no-auth/access-control policy.
            match connect_x11(source, Vec::new(), Vec::new()) {
                Ok(connection) => {
                    log::debug!(
                        "X11 endpoint {} accepted an unauthenticated setup after the configured Xauthority failed",
                        source.display()
                    );
                    Ok(connection)
                }
                Err(without_authority_error) => Err(format!(
                    "X11 setup through {} failed with configured Xauthority ({with_authority_error}); unauthenticated retry also failed ({without_authority_error})",
                    source.display()
                )),
            }
        }
        Err(error) => Err(error),
    }
}

pub(super) fn connect_x11(
    source: &Path,
    auth_name: Vec<u8>,
    auth_data: Vec<u8>,
) -> Result<AuthenticatedX11Connection, String> {
    let deadline = Instant::now() + ENDPOINT_IO_TIMEOUT;
    let stream = connect_socket(source, deadline)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    let peer = peer_credentials(&stream)?;
    let (stream, _) = DefaultStream::from_unix_stream(stream)
        .map_err(|error| format!("prepare X11 connection: {error}"))?;
    let connection = setup_x11(source, stream, auth_name, auth_data, deadline)?;
    Ok((connection, peer))
}

fn setup_x11(
    source: &Path,
    stream: DefaultStream,
    auth_name: Vec<u8>,
    auth_data: Vec<u8>,
    deadline: Instant,
) -> Result<X11Connection, String> {
    RustConnection::connect_to_stream_with_auth_info(
        EndpointStream {
            inner: stream,
            deadline: Mutex::new(deadline),
        },
        0,
        auth_name,
        auth_data,
    )
    .map_err(|error| format!("X11 setup through {} failed: {error}", source.display()))
}

pub(super) fn inspect_endpoint_peer_only(
    display: u16,
    alternate: bool,
    source: PathBuf,
) -> Result<HostX11Socket, String> {
    let canonical_path = fs::canonicalize(&source)
        .map_err(|error| format!("resolve {}: {error}", source.display()))?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|error| format!("inspect {}: {error}", canonical_path.display()))?;
    if !metadata.file_type().is_socket() {
        return Err(format!("{} is not a Unix socket", source.display()));
    }
    let stream = connect_socket(&source, Instant::now() + ENDPOINT_IO_TIMEOUT)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    let peer = peer_credentials(&stream)?;
    drop(stream);
    let (peer_pid, peer_uid, peer_gid) = peer.legacy_tuple();
    let socket = HostX11Socket::from_verified_parts(
        display,
        alternate,
        source,
        canonical_path,
        metadata.uid(),
        metadata.gid(),
        metadata.permissions().mode(),
        peer_pid,
        peer_uid,
        peer_gid,
        X11SocketRevision {
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        },
    )
    .map_err(|error| format!("X11 endpoint evidence is invalid: {error}"))?;
    require_current_socket(&socket, "during peer observation")?;
    Ok(socket)
}

pub(super) fn peer_credentials(stream: &UnixStream) -> Result<X11PeerIdentity, String> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(credentials).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(format!(
            "read X11 peer credentials: {}",
            std::io::Error::last_os_error()
        ));
    }
    let pid = u32::try_from(credentials.pid).map_err(|_| "X11 peer PID is negative")?;
    X11PeerIdentity::from_raw(pid, credentials.uid, credentials.gid)
        .map_err(|error| format!("read X11 peer credentials: {error}"))
}

pub(super) fn x11_peer_start_time(socket: &HostX11Socket) -> Result<u64, String> {
    let Some(pid) = socket.peer_identity().pid() else {
        return Err(
            "X11 server peer is outside the current PID namespace; server generation is not trackable"
                .into(),
        );
    };
    crate::adapters::process_identity::process_start_time(Path::new(&format!("/proc/{pid}")))
        .map(std::num::NonZeroU64::get)
        .map_err(|error| format!("read X server process identity for pid {pid}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathname_address_keeps_filesystem_nul_termination() {
        let (sockaddr, length) = sockaddr_for(Path::new("/tmp/.X11-unix/X0")).unwrap();
        assert_eq!(sockaddr.sun_family, libc::AF_UNIX as libc::sa_family_t);
        assert_eq!(sockaddr.sun_path[0] as u8, b'/');
        let path = b"/tmp/.X11-unix/X0";
        assert_eq!(sockaddr.sun_path[path.len()] as u8, 0);
        assert_eq!(
            length as usize,
            std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1
        );
    }

    #[test]
    fn pathname_address_rejects_invalid_paths() {
        for path in ["", "/tmp/X\0bad", &"x".repeat(108)] {
            assert_eq!(
                sockaddr_for(Path::new(path)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(sockaddr_for(Path::new(&"x".repeat(107))).is_ok());
    }

    #[test]
    fn filesystem_socket_metadata_matches_its_recorded_permissions() {
        // Keep the real socket out of the desktop's /tmp/.X11-unix directory.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("X0");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        let metadata = fs::metadata(&canonical).unwrap();
        let socket = HostX11Socket::from_verified_parts(
            0,
            false,
            "/tmp/.X11-unix/X0".into(),
            canonical.clone(),
            metadata.uid(),
            metadata.gid(),
            metadata.permissions().mode(),
            std::process::id(),
            metadata.uid(),
            metadata.gid(),
            X11SocketRevision {
                device: metadata.dev(),
                inode: metadata.ino(),
                ctime_seconds: metadata.ctime(),
                ctime_nanoseconds: metadata.ctime_nsec(),
            },
        )
        .unwrap();

        assert_eq!(socket.mode(), 0o755);
        assert!(socket_metadata_matches(&socket, &canonical, &metadata));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!socket_metadata_matches(
            &socket,
            &canonical,
            &fs::metadata(&path).unwrap()
        ));

        fs::rename(&path, temporary.path().join("previous-X0")).unwrap();
        let _replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!socket_metadata_matches(
            &socket,
            &canonical,
            &fs::metadata(&path).unwrap()
        ));
    }
}
