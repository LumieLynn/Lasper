//! Bounded discovery of local filesystem X11 endpoints. Discovery runs in the
//! invoking desktop process and never through the privileged daemon.

use std::collections::BTreeSet;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11DesktopAccessError, X11DesktopAccessPort, X11EndpointCatalog,
    X11EndpointDiscoveryPort,
};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

const X11_SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";
const ENDPOINT_IO_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_DISCOVERED_DISPLAYS: usize = 16;
const MAX_ACL_ENTRIES: usize = 1024;
const MAX_ACL_BYTES: usize = 64 * 1024;

type AuthenticatedX11Connection = (RustConnection<DefaultStream>, (u32, u32, u32));

pub(crate) struct HostX11EndpointDiscovery;

pub(crate) struct HostX11DesktopAccess;

#[async_trait::async_trait]
impl X11EndpointDiscoveryPort for HostX11EndpointDiscovery {
    async fn discover(&self) -> X11EndpointCatalog {
        match tokio::task::spawn_blocking(discover_sync).await {
            Ok(catalog) => catalog,
            Err(error) => X11EndpointCatalog {
                diagnostics: vec![format!(
                    "X11 endpoint discovery stopped unexpectedly: {error}"
                )],
                ..Default::default()
            },
        }
    }
}

#[async_trait::async_trait]
impl X11DesktopAccessPort for HostX11DesktopAccess {
    async fn snapshot(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11AclSnapshot, X11DesktopAccessError> {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || snapshot_acl_sync(&socket))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 ACL query task failed: {error}"))
            })?
            .map_err(X11DesktopAccessError::new)
    }
}

/// Re-run the authenticated X11 setup from the invoking desktop process and
/// require the endpoint evidence to remain byte-for-byte current.
pub(crate) async fn revalidate_for_desktop(socket: &HostX11Socket) -> Result<(), String> {
    let socket = socket.clone();
    tokio::task::spawn_blocking(move || {
        let current = inspect_endpoint(
            socket.display(),
            socket.alternate(),
            socket.source().to_path_buf(),
        )?;
        if current != socket {
            return Err(format!(
                "{} changed after X11 endpoint discovery",
                socket.source().display()
            ));
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("X11 endpoint revalidation task failed: {error}"))?
}

/// Revalidate filesystem and peer-process evidence without relying on the
/// caller's Xauthority. The privileged side uses this after an authenticated
/// client has already completed `revalidate_for_desktop`.
pub(crate) async fn projection_socket_identities(
    socket: &HostX11Socket,
) -> Result<Vec<(u64, u64)>, String> {
    let socket = socket.clone();
    tokio::task::spawn_blocking(move || {
        let selected = inspect_endpoint_peer_only(
            socket.display(),
            socket.alternate(),
            socket.source().to_path_buf(),
        )?;
        if selected != socket {
            return Err(format!(
                "{} changed after X11 endpoint discovery",
                socket.source().display()
            ));
        }
        let mut identities = vec![(socket.revision().device, socket.revision().inode)];
        if socket.alternate() {
            let standard = inspect_endpoint_peer_only(
                socket.display(),
                false,
                Path::new(X11_SOCKET_DIRECTORY).join(format!("X{}", socket.display())),
            )?;
            if standard.peer_identity() != socket.peer_identity() {
                return Err(format!(
                    "standard and alternate endpoints for :{} no longer belong to the same X server",
                    socket.display()
                ));
            }
            let identity = (standard.revision().device, standard.revision().inode);
            if !identities.contains(&identity) {
                identities.push(identity);
            }
        }
        Ok(identities)
    })
    .await
    .map_err(|error| format!("X11 projection evidence task failed: {error}"))?
}

fn discover_sync() -> X11EndpointCatalog {
    let preferred_display = std::env::var("DISPLAY")
        .ok()
        .and_then(|display| parse_local_display(&display));
    let mut catalog = X11EndpointCatalog {
        preferred_display,
        ..Default::default()
    };
    let directory = Path::new(X11_SOCKET_DIRECTORY);
    if let Err(reason) = validate_socket_directory(directory) {
        catalog.diagnostics.push(reason);
        return catalog;
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            catalog
                .diagnostics
                .push(format!("Cannot enumerate {}: {error}", directory.display()));
            return catalog;
        }
    };
    let mut displays = BTreeSet::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(number) = parse_standard_socket_name(&name) else {
            continue;
        };
        displays.insert(number);
    }

    let mut displays = displays.into_iter().collect::<Vec<_>>();
    if let Some(preferred) = preferred_display {
        if let Some(index) = displays.iter().position(|display| *display == preferred) {
            let preferred = displays.remove(index);
            displays.insert(0, preferred);
        }
    }
    displays.truncate(MAX_DISCOVERED_DISPLAYS);

    for display in displays {
        let standard_path = directory.join(format!("X{display}"));
        let standard = match inspect_endpoint(display, false, standard_path) {
            Ok(standard) => standard,
            Err(error) => {
                if preferred_display == Some(display) {
                    catalog.diagnostics.push(format!(
                        "Current DISPLAY :{display} is unavailable: {error}"
                    ));
                }
                continue;
            }
        };
        let peer = standard.peer_identity();
        catalog.sockets.push(standard);

        let alternate_path = directory.join(format!("X{display}_"));
        if let Ok(alternate) = inspect_endpoint(display, true, alternate_path) {
            if alternate.peer_identity() == peer {
                catalog.sockets.push(alternate);
            } else {
                catalog.diagnostics.push(format!(
                    ":{display} alternate endpoint belongs to a different server process and was ignored"
                ));
            }
        }
    }
    catalog.sockets.sort_by_key(|socket| {
        (
            socket.display() != preferred_display.unwrap_or(u16::MAX),
            socket.display(),
            socket.alternate(),
        )
    });
    catalog
}

fn parse_local_display(value: &str) -> Option<u16> {
    let parsed = parse_display::parse_display_with_file_exists_callback(value, |_| false).ok()?;
    (parsed.host.is_empty() && matches!(parsed.protocol.as_deref(), None | Some("unix")))
        .then_some(parsed.display)
}

fn parse_standard_socket_name(name: &str) -> Option<u16> {
    let number = name.strip_prefix('X')?;
    if number.is_empty()
        || number.ends_with('_')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

fn validate_socket_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    let mode = metadata.permissions().mode();
    if !metadata.is_dir() || metadata.uid() != 0 {
        return Err(format!("{} is not a root-owned directory", path.display()));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(format!(
            "{} is writable by other users without the sticky bit",
            path.display()
        ));
    }
    Ok(())
}

fn inspect_endpoint(
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

    let (connection, peer) = authenticated_connection(&source, display)?;
    let _ = connection.setup();

    let final_canonical = fs::canonicalize(&source)
        .map_err(|error| format!("re-resolve {}: {error}", source.display()))?;
    let final_metadata = fs::metadata(&final_canonical)
        .map_err(|error| format!("re-inspect {}: {error}", final_canonical.display()))?;
    if final_canonical != canonical_path
        || final_metadata.dev() != metadata.dev()
        || final_metadata.ino() != metadata.ino()
        || final_metadata.ctime() != metadata.ctime()
        || final_metadata.ctime_nsec() != metadata.ctime_nsec()
    {
        return Err(format!("{} changed during X11 discovery", source.display()));
    }

    HostX11Socket::from_verified_parts(
        display,
        alternate,
        source,
        canonical_path,
        metadata.uid(),
        metadata.gid(),
        metadata.permissions().mode(),
        peer.0,
        peer.1,
        peer.2,
        X11SocketRevision {
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        },
    )
    .map_err(|error| format!("X11 endpoint evidence is invalid: {error}"))
}

fn snapshot_acl_sync(socket: &HostX11Socket) -> Result<X11AclSnapshot, String> {
    require_current_socket(socket, "before querying its ACL")?;
    let (connection, _) = authenticated_connection(socket.source(), socket.display())?;
    let reply = connection
        .list_hosts()
        .map_err(|error| format!("send ListHosts to :{}: {error}", socket.display()))?
        .reply()
        .map_err(|error| format!("read ListHosts from :{}: {error}", socket.display()))?;
    require_current_socket(socket, "while querying its ACL")?;

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

fn require_current_socket(socket: &HostX11Socket, phase: &str) -> Result<(), String> {
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

fn authenticated_connection(
    source: &Path,
    display: u16,
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
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)
        .map_err(|error| format!("prepare X11 connection: {error}"))?;
    let authority = xauth::get_auth(family, &address, display)
        .ok()
        .flatten()
        .unwrap_or_default();
    let connection =
        RustConnection::connect_to_stream_with_auth_info(stream, 0, authority.0, authority.1)
            .map_err(|error| format!("X11 setup through {} failed: {error}", source.display()))?;
    Ok((connection, peer))
}

fn inspect_endpoint_peer_only(
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
    HostX11Socket::from_verified_parts(
        display,
        alternate,
        source,
        canonical_path,
        metadata.uid(),
        metadata.gid(),
        metadata.permissions().mode(),
        peer.0,
        peer.1,
        peer.2,
        X11SocketRevision {
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        },
    )
    .map_err(|error| format!("X11 endpoint evidence is invalid: {error}"))
}

fn peer_credentials(stream: &UnixStream) -> Result<(u32, u32, u32), String> {
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
    Ok((pid, credentials.uid, credentials.gid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_socket_names_are_kept_distinct() {
        assert_eq!(parse_local_display(":0"), Some(0));
        assert_eq!(parse_local_display("unix/:12.3"), Some(12));
        assert_eq!(parse_local_display("host:0"), None);
        assert_eq!(parse_standard_socket_name("X0"), Some(0));
        assert_eq!(parse_standard_socket_name("X12"), Some(12));
        assert_eq!(parse_standard_socket_name("X0_"), None);
        assert_eq!(parse_standard_socket_name("X"), None);
    }
}
