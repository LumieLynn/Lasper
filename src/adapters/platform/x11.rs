//! Bounded discovery of local filesystem X11 endpoints. Discovery runs in the
//! invoking desktop process and never through the privileged daemon.

use std::collections::BTreeSet;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, Family, HostMode};
use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11AuthorizationDisposition, X11AuthorizationRequest,
    X11DesktopAccessError, X11DesktopAccessPort, X11DesktopAuthorization, X11EndpointCatalog,
    X11EndpointDiscoveryPort,
};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

const X11_SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";
const ENDPOINT_IO_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_DISCOVERED_DISPLAYS: usize = 16;
const MAX_ACL_ENTRIES: usize = 1024;
const MAX_ACL_BYTES: usize = 64 * 1024;
const GRANT_RECORD_VERSION: u32 = 1;

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

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError> {
        let request = request.clone();
        tokio::task::spawn_blocking(move || ensure_access_sync(&request))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 authorization task failed: {error}"))
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
    let snapshot = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "while querying its ACL")?;
    Ok(snapshot)
}

fn read_acl(
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
enum GrantRecordPhase {
    Pending,
    ConfirmedAdded,
    OutcomeUnknown { reason: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedX11GrantRecord {
    version: u32,
    record_id: String,
    phase: GrantRecordPhase,
    created_unix_millis: u64,
    caller_uid: u32,
    boot_id: String,
    machine: String,
    guest_user: String,
    guest_uid: u32,
    guest_gid: u32,
    host_uid: u32,
    host_gid: u32,
    machine_leader_pid: u32,
    machine_pid_namespace: (u64, u64),
    machine_user_namespace: (u64, u64),
    display: u16,
    alternate_endpoint: bool,
    source: PathBuf,
    canonical_source: PathBuf,
    socket_revision: X11SocketRevision,
    server_peer: (u32, u32, u32),
    server_peer_start_time: u64,
    acl_family: u8,
    acl_address: Vec<u8>,
}

impl ManagedX11GrantRecord {
    fn pending(
        request: &X11AuthorizationRequest,
        record_id: String,
        server_peer_start_time: u64,
    ) -> Result<Self, String> {
        let projection = request.projection();
        let identity = projection.identity();
        let instance = identity.instance();
        let pid_namespace = instance.pid_namespace();
        let user_namespace = instance.user_namespace();
        let host_uid = identity.host_uid();
        Ok(Self {
            version: GRANT_RECORD_VERSION,
            record_id,
            phase: GrantRecordPhase::Pending,
            created_unix_millis: unix_millis()?,
            caller_uid: uzers::get_effective_uid(),
            boot_id: host_boot_id()?,
            machine: request.target().machine().as_str().to_owned(),
            guest_user: request.target().user().as_str().to_owned(),
            guest_uid: identity.guest().uid(),
            guest_gid: identity.guest().gid(),
            host_uid,
            host_gid: identity.host_gid(),
            machine_leader_pid: instance.leader_pid(),
            machine_pid_namespace: (pid_namespace.device(), pid_namespace.inode()),
            machine_user_namespace: (user_namespace.device(), user_namespace.inode()),
            display: projection.host_socket().display(),
            alternate_endpoint: projection.host_socket().alternate(),
            source: projection.host_socket().source().to_path_buf(),
            canonical_source: projection.host_socket().canonical_path().to_path_buf(),
            socket_revision: projection.host_socket().revision(),
            server_peer: projection.host_socket().peer_identity(),
            server_peer_start_time,
            acl_family: Family::SERVER_INTERPRETED.into(),
            acl_address: numeric_local_user_address(host_uid),
        })
    }
}

struct X11RuntimeState {
    access: crate::adapters::trusted_state::TrustedDirectory,
    grants: crate::adapters::trusted_state::TrustedDirectory,
}

impl X11RuntimeState {
    fn open() -> Result<Self, String> {
        let uid = uzers::get_effective_uid();
        let runtime = user_runtime_directory(uid)?;
        let runtime =
            crate::adapters::trusted_state::TrustedDirectory::open_existing(&runtime, uid)
                .map_err(|error| format!("open user runtime directory: {error}"))?;
        let lasper = runtime
            .open_or_create_child("lasper", 0o700)
            .map_err(|error| format!("open Lasper runtime directory: {error}"))?;
        let access = lasper
            .open_or_create_child("x11-access", 0o700)
            .map_err(|error| format!("open X11 access runtime directory: {error}"))?;
        let grants = access
            .open_or_create_child("grants", 0o700)
            .map_err(|error| format!("open X11 grant record directory: {error}"))?;
        Ok(Self { access, grants })
    }

    fn lock(&self) -> Result<std::fs::File, String> {
        self.access
            .lock_exclusive("acl")
            .map_err(|error| format!("lock X11 access operations: {error}"))
    }

    fn write(&self, file_name: &str, record: &ManagedX11GrantRecord) -> Result<(), String> {
        let bytes = serde_json::to_vec(record)
            .map_err(|error| format!("serialize X11 grant record: {error}"))?;
        self.grants
            .write_atomic(file_name, &bytes, 0o600)
            .map_err(|error| format!("persist X11 grant record: {error}"))
    }
}

fn ensure_access_sync(
    request: &X11AuthorizationRequest,
) -> Result<X11DesktopAuthorization, String> {
    let state = X11RuntimeState::open()?;
    let _lock = state.lock()?;
    let projection = request.projection();
    let socket = projection.host_socket();
    require_current_socket(socket, "before authorizing access")?;
    let server_peer_start_time = x11_peer_start_time(socket)?;
    let (connection, _) = authenticated_connection(socket.source(), socket.display())?;
    let before = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "before changing its ACL")?;

    match before.mode() {
        crate::application::x11::X11AccessControlMode::Disabled => {
            return Ok(X11DesktopAuthorization::new(
                before,
                X11AuthorizationDisposition::AccessControlDisabled,
            ));
        }
        crate::application::x11::X11AccessControlMode::Unknown(mode) => {
            return Err(format!(
                "X server returned unsupported access-control mode {mode}; no ACL change was attempted"
            ));
        }
        crate::application::x11::X11AccessControlMode::Enabled => {}
    }

    let host_uid = projection.identity().host_uid();
    if before.has_numeric_local_user(host_uid) {
        return Ok(X11DesktopAuthorization::new(
            before,
            X11AuthorizationDisposition::PreExisting,
        ));
    }

    let record_id = uuid::Uuid::new_v4().simple().to_string();
    let file_name = format!("grant-{record_id}.json");
    let mut record =
        ManagedX11GrantRecord::pending(request, record_id.clone(), server_peer_start_time)?;
    state.write(&file_name, &record)?;

    let (change_result, observed) = insert_and_observe(&connection, socket.display(), host_uid);

    let socket_result = require_current_socket(socket, "while authorizing access");
    let server_result = x11_peer_start_time(socket).and_then(|current| {
        (current == server_peer_start_time)
            .then_some(())
            .ok_or_else(|| "X server process changed while authorizing access".to_owned())
    });
    if authorization_was_confirmed(
        &change_result,
        &observed,
        &socket_result,
        &server_result,
        host_uid,
    ) {
        let after = observed.expect("confirmed observation is successful");
        record.phase = GrantRecordPhase::ConfirmedAdded;
        state.write(&file_name, &record).map_err(|error| {
            format!(
                "X11 access was added, but its operation record could not be finalized ({error}); the pending record was preserved"
            )
        })?;
        return Ok(X11DesktopAuthorization::new(
            after,
            X11AuthorizationDisposition::Added { record_id },
        ));
    }

    let reason = [
        change_result.err(),
        observed
            .err()
            .map(|error| format!("confirmation query: {error}")),
        socket_result
            .err()
            .map(|error| format!("endpoint revalidation: {error}")),
        server_result
            .err()
            .map(|error| format!("server generation: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");
    let reason = if reason.is_empty() {
        format!("localuser:#{host_uid} was absent after the X server round trip")
    } else {
        reason
    };
    record.phase = GrantRecordPhase::OutcomeUnknown {
        reason: reason.clone(),
    };
    let record_result = state.write(&file_name, &record);
    Err(match record_result {
        Ok(()) => format!(
            "X11 authorization outcome is unknown: {reason}; operation record {record_id} was preserved"
        ),
        Err(record_error) => format!(
            "X11 authorization outcome is unknown: {reason}; additionally, the pending operation record could not be updated: {record_error}"
        ),
    })
}

fn insert_and_observe(
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

fn authorization_was_confirmed(
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

fn numeric_local_user_address(uid: u32) -> Vec<u8> {
    format!("localuser\0#{uid}").into_bytes()
}

fn user_runtime_directory(uid: u32) -> Result<PathBuf, String> {
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

fn host_boot_id() -> Result<String, String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read host boot identity: {error}"))?;
    uuid::Uuid::parse_str(value.trim())
        .map(|value| value.to_string())
        .map_err(|error| format!("parse host boot identity: {error}"))
}

fn unix_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system time: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "system time exceeds the X11 record format".into())
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

fn x11_peer_start_time(socket: &HostX11Socket) -> Result<u64, String> {
    let (pid, _, _) = socket.peer_identity();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance,
        ObservedNamespaceIdentity, ShellTarget, ValidatedGuestUserName, X11ProjectionContext,
    };
    use crate::domain::x11::X11SocketRevision;

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

    #[test]
    fn an_observed_entry_is_not_claimed_when_change_hosts_failed() {
        let uid = 1_437_402_088;
        let observed = Ok(X11AclSnapshot::from_wire(
            1,
            vec![X11AclEntry::from_wire(
                Family::SERVER_INTERPRETED.into(),
                numeric_local_user_address(uid),
            )],
        ));
        let endpoint = Ok(());
        let server = Ok(());

        assert!(!authorization_was_confirmed(
            &Err("external race".into()),
            &observed,
            &endpoint,
            &server,
            uid,
        ));
        assert!(authorization_was_confirmed(
            &Ok(()),
            &observed,
            &endpoint,
            &server,
            uid,
        ));
    }

    #[test]
    fn grant_records_are_versioned_and_reject_unknown_fields() {
        let namespace = ObservedNamespaceIdentity::new(1, 2);
        let identity = MappedGuestIdentity::verified(
            ObservedGuestIdentity::new(1000, 1000),
            1_437_402_088,
            1_437_402_088,
            ObservedMachineInstance::new(42, namespace, namespace),
        );
        let socket = HostX11Socket::from_verified_parts(
            0,
            false,
            "/tmp/.X11-unix/X0".into(),
            "/tmp/.X11-unix/X0".into(),
            1000,
            1000,
            0o755,
            42,
            1000,
            1000,
            X11SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        let projection = X11ProjectionContext::verified(
            socket,
            "/mnt/host-x11/X0".into(),
            "/tmp/.X11-unix/X0".into(),
            identity,
        );
        let request = X11AuthorizationRequest::new(
            ShellTarget::new(
                crate::domain::machine::MachineName::new("archlinux").unwrap(),
                ValidatedGuestUserName::new("alice").unwrap(),
            ),
            projection,
        );
        let record = ManagedX11GrantRecord::pending(&request, "record-1".into(), 77).unwrap();
        let value = serde_json::to_value(&record).unwrap();
        let decoded: ManagedX11GrantRecord = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.record_id, "record-1");
        assert_eq!(decoded.server_peer_start_time, 77);
        assert_eq!(decoded.acl_address, b"localuser\0#1437402088".to_vec());

        let mut unknown = value;
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ManagedX11GrantRecord>(unknown).is_err());
    }
}
