//! X11 wire transport and endpoint revalidation primitives.
//!
//! This module owns Unix-socket connections, Xauthority fallback, ACL wire
//! encoding, peer credentials, and replacement/generation checks. Higher
//! modules decide when a transport operation is allowed.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use x11rb::protocol::xproto::{ConnectionExt, Family, HostMode};
use x11rb::reexports::x11rb_protocol::xauth;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::application::x11::{X11AclEntry, X11AclSnapshot};
use crate::domain::x11::{HostX11Socket, X11PeerIdentity, X11SocketRevision};

use super::common::{ENDPOINT_IO_TIMEOUT, MAX_ACL_BYTES, MAX_ACL_ENTRIES};

type AuthenticatedX11Connection = (RustConnection<DefaultStream>, X11PeerIdentity);

pub(super) fn read_acl(
    connection: &RustConnection<DefaultStream>,
    display: u16,
) -> Result<X11AclSnapshot, String> {
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
    connection: &RustConnection<DefaultStream>,
    display: u16,
    host_uid: u32,
) -> (Result<(), String>, Result<X11AclSnapshot, String>) {
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
    connection: &RustConnection<DefaultStream>,
    display: u16,
    host_uid: u32,
) -> (Result<(), String>, Result<X11AclSnapshot, String>) {
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
    let current = inspect_endpoint_peer_only(
        socket.display(),
        socket.alternate(),
        socket.source().to_path_buf(),
    )?;
    if &current != socket {
        return Err(format!("{} changed {phase}", socket.source().display()));
    }
    Ok(())
}

pub(super) fn authenticated_connection(
    source: &Path,
    display: u16,
) -> Result<AuthenticatedX11Connection, String> {
    // The family/address used for Xauthority lookup is the same one that
    // x11rb derives from a freshly connected Unix stream.
    let stream = UnixStream::connect(source)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)
        .map_err(|error| format!("prepare X11 authority lookup: {error}"))?;
    drop(stream);
    let authority = xauth::get_auth(family, &address, display)
        .ok()
        .flatten()
        .unwrap_or_default();

    match connect_x11(source, authority.0.clone(), authority.1.clone()) {
        Ok(connection) => Ok(connection),
        Err(with_authority_error) if !authority.0.is_empty() || !authority.1.is_empty() => {
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
    let stream = UnixStream::connect(source)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    stream
        .set_read_timeout(Some(ENDPOINT_IO_TIMEOUT))
        .map_err(|error| format!("set X11 read deadline: {error}"))?;
    stream
        .set_write_timeout(Some(ENDPOINT_IO_TIMEOUT))
        .map_err(|error| format!("set X11 write deadline: {error}"))?;
    let peer = peer_credentials(&stream)?;
    let (stream, _) = DefaultStream::from_unix_stream(stream)
        .map_err(|error| format!("prepare X11 connection: {error}"))?;
    let connection =
        RustConnection::connect_to_stream_with_auth_info(stream, 0, auth_name, auth_data)
            .map_err(|error| format!("X11 setup through {} failed: {error}", source.display()))?;
    Ok((connection, peer))
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
    let stream = UnixStream::connect(&source)
        .map_err(|error| format!("connect to {}: {error}", source.display()))?;
    let peer = peer_credentials(&stream)?;
    drop(stream);
    let (peer_pid, peer_uid, peer_gid) = peer.legacy_tuple();
    HostX11Socket::from_verified_parts(
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
    .map_err(|error| format!("X11 endpoint evidence is invalid: {error}"))
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
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read X server process identity for pid {pid}: {error}"))?;
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields)
        .ok_or_else(|| format!("X server process {pid} stat has no command terminator"))?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| format!("X server process {pid} stat has no start time"))?
        .parse()
        .map_err(|error| format!("X server process {pid} start time is invalid: {error}"))
}
