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

use crate::application::sessions::{
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    ShellTarget, ValidatedGuestUserName,
};
use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11AuthorizationDisposition, X11AuthorizationPurpose,
    X11AuthorizationRequest, X11DesktopAccessError, X11DesktopAccessPort, X11DesktopAuthorization,
    X11DesktopObservation, X11DesktopRevocation, X11EndpointCatalog, X11EndpointDiscoveryPort,
    X11GrantRecordCatalog, X11GrantRecordEvidence, X11GrantRecordPhase, X11RevocationDisposition,
    X11RevokeRequest, X11SourceObservation, X11SourceState,
};
use crate::domain::machine::MachineName;
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

const X11_SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";
const ENDPOINT_IO_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_DISCOVERED_DISPLAYS: usize = 16;
const MAX_INSPECTED_SOURCES: usize = 64;
const MAX_ACL_ENTRIES: usize = 1024;
const MAX_ACL_BYTES: usize = 64 * 1024;
const GRANT_RECORD_VERSION: u32 = 1;
const MAX_GRANT_RECORDS: usize = 256;
const MAX_GRANT_RECORD_BYTES: usize = 16 * 1024;
const MAX_GRANT_DIAGNOSTICS: usize = 16;
const MAX_GRANT_REASON_BYTES: usize = 2048;
const MACHINE_CLAIM_VERSION: u32 = 1;
const MAX_MACHINE_CLAIMS: usize = 256;
const MAX_MACHINE_CLAIM_BYTES: usize = 16 * 1024;
const CLAIM_RECONCILE_GRACE_MILLIS: u64 = 2_000;

type AuthenticatedX11Connection = (RustConnection<DefaultStream>, (u32, u32, u32));

pub(crate) struct HostX11EndpointDiscovery;

pub(crate) struct HostX11DesktopAccess;

#[async_trait::async_trait]
impl X11EndpointDiscoveryPort for HostX11EndpointDiscovery {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        let configured_sources = configured_sources.to_vec();
        match tokio::task::spawn_blocking(move || discover_sync(&configured_sources)).await {
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
    ) -> Result<X11DesktopObservation, X11DesktopAccessError> {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || snapshot_desktop_sync(&socket))
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

    async fn revoke(
        &self,
        request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError> {
        let request = request.clone();
        tokio::task::spawn_blocking(move || revoke_access_sync(&request))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 revocation task failed: {error}"))
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

fn discover_sync(configured_sources: &[PathBuf]) -> X11EndpointCatalog {
    let preferred_display = std::env::var("DISPLAY")
        .ok()
        .and_then(|display| parse_local_display(&display));
    let mut catalog = X11EndpointCatalog {
        preferred_display,
        ..Default::default()
    };
    let directory = Path::new(X11_SOCKET_DIRECTORY);
    let mut sources = BTreeSet::new();
    for source in configured_sources {
        if sources.contains(source) {
            continue;
        }
        if sources.len() == MAX_INSPECTED_SOURCES {
            catalog.diagnostics.push(format!(
                "Only {MAX_INSPECTED_SOURCES} configured X11 sources were inspected; remaining sources are unverified."
            ));
            break;
        }
        sources.insert(source.clone());
        catalog.sources.push(X11SourceObservation {
            source: source.clone(),
            state: inspect_source(source, directory),
        });
    }
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
                record_endpoint_failure(
                    &mut catalog,
                    &directory.join(format!("X{display}")),
                    &error,
                );
                continue;
            }
        };
        let peer = standard.peer_identity();
        catalog.sockets.push(standard);

        let alternate_path = directory.join(format!("X{display}_"));
        match inspect_endpoint(display, true, alternate_path.clone()) {
            Ok(alternate) if alternate.peer_identity() == peer => {
                catalog.sockets.push(alternate);
            }
            Ok(_) => record_endpoint_failure(
                &mut catalog,
                &alternate_path,
                "alternate endpoint does not belong to the standard endpoint's peer process",
            ),
            Err(error) if sources.contains(&alternate_path) => {
                record_endpoint_failure(&mut catalog, &alternate_path, &error);
            }
            Err(_) => {}
        }
    }
    for observation in &mut catalog.sources {
        if catalog.sockets.iter().any(|socket| {
            socket.source() == observation.source
                || (observation.source == directory && socket.source().parent() == Some(directory))
        }) {
            observation.state = X11SourceState::Observed;
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

fn inspect_source(source: &Path, directory: &Path) -> X11SourceState {
    let is_directory = source == directory;
    let is_socket_path = source.parent() == Some(directory)
        && source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                parse_standard_socket_name(name.strip_suffix('_').unwrap_or(name)).is_some()
            });
    if !is_directory && !is_socket_path {
        return X11SourceState::Unverified("Source is outside local X11 discovery scope.".into());
    }
    match fs::metadata(source) {
        Ok(metadata) if is_directory && metadata.is_dir() => X11SourceState::Unverified(
            "Directory exists, but no authenticated X11 endpoint was observed inside it. See Checks for discovery diagnostics.".into(),
        ),
        Ok(metadata) if !is_directory && metadata.file_type().is_socket() => X11SourceState::Unverified(
            "Socket exists, but its X11 endpoint has not been verified. See Checks for discovery diagnostics.".into(),
        ),
        Ok(_) => X11SourceState::Invalid(format!(
            "Expected a {} at this source.",
            if is_directory { "directory" } else { "Unix socket" },
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => X11SourceState::Missing,
        Err(error) => X11SourceState::Unverified(format!("Cannot inspect source: {error}")),
    }
}

fn record_endpoint_failure(catalog: &mut X11EndpointCatalog, source: &Path, reason: &str) {
    catalog
        .diagnostics
        .push(format!("{}: {reason}", source.display()));
    if let Some(observation) = catalog
        .sources
        .iter_mut()
        .find(|entry| entry.source == source)
    {
        if matches!(observation.state, X11SourceState::Unverified(_)) {
            observation.state = X11SourceState::Unverified(reason.to_owned());
        }
    }
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

fn snapshot_desktop_sync(socket: &HostX11Socket) -> Result<X11DesktopObservation, String> {
    require_current_socket(socket, "before querying its ACL")?;
    let (connection, _) = authenticated_connection(socket.source(), socket.display())?;
    let acl = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "while querying its ACL")?;
    let observation = desktop_observation(socket, acl, None);
    require_current_socket(socket, "while assessing its grant records")?;
    Ok(observation)
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
    Revoked,
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

    fn into_evidence(self, file_name: &str) -> Result<X11GrantRecordEvidence, String> {
        if self.version != GRANT_RECORD_VERSION {
            return Err(format!("unsupported record version {}", self.version));
        }
        let expected_name = format!("grant-{}.json", self.record_id);
        if file_name != expected_name || !valid_record_id(&self.record_id) {
            return Err("record ID does not match its filename".into());
        }
        let boot_id = uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|error| format!("invalid host boot identity: {error}"))?
            .to_string();
        let machine = MachineName::new(self.machine)
            .map_err(|error| format!("invalid machine name: {error}"))?;
        let guest_user = ValidatedGuestUserName::new(self.guest_user)
            .map_err(|error| format!("invalid guest user: {error}"))?;
        if self.machine_leader_pid == 0
            || self.machine_leader_pid > i32::MAX as u32
            || self.machine_pid_namespace.1 == 0
            || self.machine_user_namespace.1 == 0
        {
            return Err("recorded machine instance is invalid".into());
        }
        if self.server_peer.0 == 0 || self.server_peer.0 > i32::MAX as u32 {
            return Err("recorded X server PID is invalid".into());
        }
        if self.server_peer_start_time == 0 {
            return Err("recorded X server start time is invalid".into());
        }
        let expected_source = Path::new(X11_SOCKET_DIRECTORY).join(format!(
            "X{}{}",
            self.display,
            if self.alternate_endpoint { "_" } else { "" }
        ));
        if self.source != expected_source
            || !self.canonical_source.is_absolute()
            || self.socket_revision.inode == 0
        {
            return Err("recorded X11 endpoint path is invalid".into());
        }
        let expected_acl = numeric_local_user_address(self.host_uid);
        if self.acl_family != u8::from(Family::SERVER_INTERPRETED)
            || self.acl_address != expected_acl
        {
            return Err("recorded ACL key is not the exact mapped numeric localuser".into());
        }
        let phase = match self.phase {
            GrantRecordPhase::Pending => X11GrantRecordPhase::Pending,
            GrantRecordPhase::ConfirmedAdded => X11GrantRecordPhase::ConfirmedAdded,
            GrantRecordPhase::Revoked => X11GrantRecordPhase::Revoked,
            GrantRecordPhase::OutcomeUnknown { reason } => {
                if reason.len() > MAX_GRANT_REASON_BYTES || reason.chars().any(char::is_control) {
                    return Err("recorded outcome reason is invalid".into());
                }
                X11GrantRecordPhase::OutcomeUnknown { reason }
            }
        };
        let pid_namespace = ObservedNamespaceIdentity::new(
            self.machine_pid_namespace.0,
            self.machine_pid_namespace.1,
        );
        let user_namespace = ObservedNamespaceIdentity::new(
            self.machine_user_namespace.0,
            self.machine_user_namespace.1,
        );
        Ok(X11GrantRecordEvidence {
            record_id: self.record_id,
            phase,
            created_unix_millis: self.created_unix_millis,
            caller_uid: self.caller_uid,
            boot_id,
            target: ShellTarget::new(machine, guest_user),
            identity: MappedGuestIdentity::verified(
                ObservedGuestIdentity::new(self.guest_uid, self.guest_gid),
                self.host_uid,
                self.host_gid,
                ObservedMachineInstance::new(
                    self.machine_leader_pid,
                    pid_namespace,
                    user_namespace,
                ),
            ),
            display: self.display,
            alternate_endpoint: self.alternate_endpoint,
            source: self.source,
            canonical_source: self.canonical_source,
            socket_revision: self.socket_revision,
            server_peer: self.server_peer,
            server_peer_start_time: self.server_peer_start_time,
            acl_entry: X11AclEntry::from_wire(self.acl_family, self.acl_address),
        })
    }
}

/// The exact desktop ACL identity owned by one Lasper grant.  A machine
/// claim references this value instead of treating a display number as an
/// ownership key: the X server generation, endpoint revision, and mapped UID
/// are all part of the key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedX11GrantKey {
    host_uid: u32,
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

impl ManagedX11GrantKey {
    fn from_record(record: &ManagedX11GrantRecord) -> Self {
        Self {
            host_uid: record.host_uid,
            display: record.display,
            alternate_endpoint: record.alternate_endpoint,
            source: record.source.clone(),
            canonical_source: record.canonical_source.clone(),
            socket_revision: record.socket_revision,
            server_peer: record.server_peer,
            server_peer_start_time: record.server_peer_start_time,
            acl_family: record.acl_family,
            acl_address: record.acl_address.clone(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let expected_source = Path::new(X11_SOCKET_DIRECTORY).join(format!(
            "X{}{}",
            self.display,
            if self.alternate_endpoint { "_" } else { "" }
        ));
        if self.source != expected_source
            || !self.canonical_source.is_absolute()
            || self.socket_revision.inode == 0
        {
            return Err("machine claim contains an invalid X11 endpoint key".into());
        }
        if self.server_peer.0 == 0 || self.server_peer.0 > i32::MAX as u32 {
            return Err("machine claim contains an invalid X server PID".into());
        }
        if self.server_peer_start_time == 0 {
            return Err("machine claim contains an invalid X server generation".into());
        }
        if self.acl_family != u8::from(Family::SERVER_INTERPRETED)
            || self.acl_address != numeric_local_user_address(self.host_uid)
        {
            return Err(
                "machine claim does not contain the exact numeric localuser ACL key".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
enum MachineClaimPhase {
    Active,
    CleanupPending { since_unix_millis: u64 },
    Ended,
    NeedsReview { reason: String },
    Unknown { reason: String },
}

/// A lifecycle claim is deliberately separate from the grant operation
/// record.  The grant record answers “what ACL mutation happened”; this file
/// answers “which currently-running system-scope machine instance may keep
/// that exact ACL key alive”.  Keeping the two records separate lets future
/// reconciliation end a claim without rewriting operation history.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedX11MachineClaim {
    version: u32,
    claim_id: String,
    grant_record_id: String,
    phase: MachineClaimPhase,
    created_unix_millis: u64,
    boot_id: String,
    machine: String,
    guest_user: String,
    guest_uid: u32,
    guest_gid: u32,
    host_gid: u32,
    machine_leader_pid: u32,
    machine_pid_namespace: (u64, u64),
    machine_user_namespace: (u64, u64),
    key: ManagedX11GrantKey,
}

impl ManagedX11MachineClaim {
    fn active_from_record(record: &ManagedX11GrantRecord) -> Self {
        Self {
            version: MACHINE_CLAIM_VERSION,
            claim_id: record.record_id.clone(),
            grant_record_id: record.record_id.clone(),
            phase: MachineClaimPhase::Active,
            created_unix_millis: record.created_unix_millis,
            boot_id: record.boot_id.clone(),
            machine: record.machine.clone(),
            guest_user: record.guest_user.clone(),
            guest_uid: record.guest_uid,
            guest_gid: record.guest_gid,
            host_gid: record.host_gid,
            machine_leader_pid: record.machine_leader_pid,
            machine_pid_namespace: record.machine_pid_namespace,
            machine_user_namespace: record.machine_user_namespace,
            key: ManagedX11GrantKey::from_record(record),
        }
    }

    fn validate(&self, file_name: &str) -> Result<(), String> {
        if self.version != MACHINE_CLAIM_VERSION {
            return Err(format!(
                "unsupported machine claim version {}",
                self.version
            ));
        }
        if !valid_record_id(&self.claim_id)
            || self.claim_id != self.grant_record_id
            || file_name != format!("claim-{}.json", self.claim_id)
            || !valid_record_id(&self.grant_record_id)
        {
            return Err("machine claim ID does not match its filename or grant record".into());
        }
        uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|error| format!("invalid machine claim host boot identity: {error}"))?;
        MachineName::new(self.machine.clone())
            .map_err(|error| format!("invalid machine claim machine name: {error}"))?;
        ValidatedGuestUserName::new(self.guest_user.clone())
            .map_err(|error| format!("invalid machine claim guest user: {error}"))?;
        if self.machine_leader_pid == 0
            || self.machine_leader_pid > i32::MAX as u32
            || self.machine_pid_namespace.1 == 0
            || self.machine_user_namespace.1 == 0
        {
            return Err("machine claim contains an invalid machine instance".into());
        }
        self.key.validate()?;
        if let MachineClaimPhase::NeedsReview { reason } | MachineClaimPhase::Unknown { reason } =
            &self.phase
        {
            if reason.is_empty()
                || reason.len() > MAX_GRANT_REASON_BYTES
                || reason.chars().any(char::is_control)
            {
                return Err("machine claim state reason is invalid".into());
            }
        }
        if let MachineClaimPhase::CleanupPending { since_unix_millis } = self.phase {
            if since_unix_millis == 0 {
                return Err("machine claim cleanup-pending timestamp is invalid".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct MachineClaimCatalog {
    claims: Vec<ManagedX11MachineClaim>,
    diagnostics: Vec<String>,
    complete: bool,
}

impl Default for MachineClaimCatalog {
    fn default() -> Self {
        Self {
            claims: Vec::new(),
            diagnostics: Vec::new(),
            complete: true,
        }
    }
}

fn valid_record_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct X11RuntimeState {
    access: crate::adapters::trusted_state::TrustedDirectory,
    grants: crate::adapters::trusted_state::TrustedDirectory,
    claims: Option<crate::adapters::trusted_state::TrustedDirectory>,
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
        let claims = access
            .open_or_create_child("claims", 0o700)
            .map_err(|error| format!("open X11 machine claim directory: {error}"))?;
        Ok(Self {
            access,
            grants,
            claims: Some(claims),
        })
    }

    fn open_existing() -> Result<Option<Self>, String> {
        let uid = uzers::get_effective_uid();
        let runtime = user_runtime_directory(uid)?;
        let runtime =
            crate::adapters::trusted_state::TrustedDirectory::open_existing(&runtime, uid)
                .map_err(|error| format!("open user runtime directory: {error}"))?;
        let Some(lasper) = runtime
            .open_existing_child("lasper")
            .map_err(|error| format!("open Lasper runtime directory: {error}"))?
        else {
            return Ok(None);
        };
        let Some(access) = lasper
            .open_existing_child("x11-access")
            .map_err(|error| format!("open X11 access runtime directory: {error}"))?
        else {
            return Ok(None);
        };
        let Some(grants) = access
            .open_existing_child("grants")
            .map_err(|error| format!("open X11 grant record directory: {error}"))?
        else {
            return Ok(None);
        };
        let claims = access
            .open_existing_child("claims")
            .map_err(|error| format!("open X11 machine claim directory: {error}"))?;
        Ok(Some(Self {
            access,
            grants,
            claims,
        }))
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

    fn write_claim(&self, claim: &ManagedX11MachineClaim) -> Result<(), String> {
        let claims = self
            .claims
            .as_ref()
            .ok_or_else(|| "X11 machine claim directory is unavailable".to_owned())?;
        let file_name = format!("claim-{}.json", claim.claim_id);
        claim.validate(&file_name)?;
        let bytes = serde_json::to_vec(claim)
            .map_err(|error| format!("serialize X11 machine claim: {error}"))?;
        claims
            .write_atomic(&file_name, &bytes, 0o600)
            .map_err(|error| format!("persist X11 machine claim: {error}"))
    }

    /// End the claim associated with one grant record. Older runtime state
    /// has no claim directory; that is an ordinary compatibility case and is
    /// intentionally treated as a no-op.
    fn end_claim(&self, grant_record_id: &str) -> Result<(), String> {
        let Some(claims) = self.claims.as_ref() else {
            return Ok(());
        };
        let file_name = format!("claim-{grant_record_id}.json");
        let Some(file) = claims
            .read_bounded(&file_name, MAX_MACHINE_CLAIM_BYTES)
            .map_err(|error| format!("read X11 machine claim {grant_record_id}: {error}"))?
        else {
            return Ok(());
        };
        if file.uid != uzers::get_effective_uid() || file.mode & 0o077 != 0 {
            return Err(format!(
                "X11 machine claim {grant_record_id} is not owned by the invoking user"
            ));
        }
        let mut claim: ManagedX11MachineClaim = serde_json::from_slice(&file.bytes)
            .map_err(|error| format!("invalid X11 machine claim {grant_record_id}: {error}"))?;
        claim
            .validate(&file_name)
            .map_err(|error| format!("invalid X11 machine claim {grant_record_id}: {error}"))?;
        claim.phase = MachineClaimPhase::Ended;
        self.write_claim(&claim)
    }

    fn load_records(&self) -> X11GrantRecordCatalog {
        load_grant_records_from(&self.grants, uzers::get_effective_uid())
    }

    fn load_claims(&self) -> MachineClaimCatalog {
        self.claims
            .as_ref()
            .map(|claims| load_machine_claims_from(claims, uzers::get_effective_uid()))
            .unwrap_or_default()
    }
}

fn load_existing_grant_records() -> X11GrantRecordCatalog {
    match X11RuntimeState::open_existing() {
        Ok(Some(state)) => state.load_records(),
        Ok(None) => X11GrantRecordCatalog::empty(),
        Err(error) => X11GrantRecordCatalog::unavailable(format!(
            "X11 grant records could not be inspected: {error}"
        )),
    }
}

fn load_existing_machine_claims() -> MachineClaimCatalog {
    match X11RuntimeState::open_existing() {
        Ok(Some(state)) => state.load_claims(),
        Ok(None) => MachineClaimCatalog::default(),
        Err(error) => MachineClaimCatalog {
            diagnostics: vec![format!(
                "X11 machine claims could not be inspected: {error}"
            )],
            complete: false,
            ..Default::default()
        },
    }
}

fn load_grant_records_from(
    grants: &crate::adapters::trusted_state::TrustedDirectory,
    expected_uid: u32,
) -> X11GrantRecordCatalog {
    let mut names = match grants.entry_names() {
        Ok(names) => names
            .into_iter()
            .filter(|name| {
                name.strip_prefix("grant-")
                    .and_then(|value| value.strip_suffix(".json"))
                    .is_some()
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return X11GrantRecordCatalog::unavailable(format!(
                "X11 grant record directory could not be listed: {error}"
            ));
        }
    };
    names.sort();
    let mut complete = true;
    let mut diagnostics = Vec::new();
    let mut omitted_diagnostics = 0usize;
    if names.len() > MAX_GRANT_RECORDS {
        complete = false;
        let excess = names.len() - MAX_GRANT_RECORDS;
        names.truncate(MAX_GRANT_RECORDS);
        push_grant_diagnostic(
            &mut diagnostics,
            &mut omitted_diagnostics,
            format!(
                "X11 grant record count exceeded {MAX_GRANT_RECORDS}; {excess} records were not read"
            ),
        );
    }

    let mut records = Vec::with_capacity(names.len());
    for name in names {
        let record = (|| {
            let file = grants
                .read_bounded(&name, MAX_GRANT_RECORD_BYTES)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "record disappeared while it was being read".to_owned())?;
            if file.uid != expected_uid || file.mode & 0o077 != 0 {
                return Err(format!(
                    "record must be owned by uid {expected_uid} and inaccessible to group/other"
                ));
            }
            serde_json::from_slice::<ManagedX11GrantRecord>(&file.bytes)
                .map_err(|error| format!("invalid record JSON: {error}"))?
                .into_evidence(&name)
        })();
        match record {
            Ok(record) => records.push(record),
            Err(error) => {
                complete = false;
                push_grant_diagnostic(
                    &mut diagnostics,
                    &mut omitted_diagnostics,
                    format!("{name:?}: {}", bounded_record_diagnostic(&error)),
                );
            }
        }
    }
    if omitted_diagnostics > 0 {
        diagnostics.push(format!(
            "{omitted_diagnostics} additional X11 grant record diagnostics were omitted"
        ));
    }
    X11GrantRecordCatalog {
        records,
        diagnostics,
        complete,
    }
}

fn load_machine_claims_from(
    claims: &crate::adapters::trusted_state::TrustedDirectory,
    expected_uid: u32,
) -> MachineClaimCatalog {
    let mut names = match claims.entry_names() {
        Ok(names) => names
            .into_iter()
            .filter(|name| {
                name.strip_prefix("claim-")
                    .and_then(|value| value.strip_suffix(".json"))
                    .is_some()
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return MachineClaimCatalog {
                diagnostics: vec![format!(
                    "X11 machine claim directory could not be listed: {error}"
                )],
                complete: false,
                ..Default::default()
            }
        }
    };
    names.sort();
    let mut complete = true;
    let mut diagnostics = Vec::new();
    let mut omitted_diagnostics = 0usize;
    if names.len() > MAX_MACHINE_CLAIMS {
        complete = false;
        let excess = names.len() - MAX_MACHINE_CLAIMS;
        names.truncate(MAX_MACHINE_CLAIMS);
        push_claim_diagnostic(
            &mut diagnostics,
            &mut omitted_diagnostics,
            format!(
                "X11 machine claim count exceeded {MAX_MACHINE_CLAIMS}; {excess} claims were not read"
            ),
        );
    }

    let mut loaded = Vec::with_capacity(names.len());
    for name in names {
        let claim = (|| {
            let file = claims
                .read_bounded(&name, MAX_MACHINE_CLAIM_BYTES)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "claim disappeared while it was being read".to_owned())?;
            if file.uid != expected_uid || file.mode & 0o077 != 0 {
                return Err(format!(
                    "claim must be owned by uid {expected_uid} and inaccessible to group/other"
                ));
            }
            let claim = serde_json::from_slice::<ManagedX11MachineClaim>(&file.bytes)
                .map_err(|error| format!("invalid claim JSON: {error}"))?;
            claim.validate(&name)?;
            Ok(claim)
        })();
        match claim {
            Ok(claim) => loaded.push(claim),
            Err(error) => {
                complete = false;
                push_claim_diagnostic(
                    &mut diagnostics,
                    &mut omitted_diagnostics,
                    format!("{name:?}: {}", bounded_record_diagnostic(&error)),
                );
            }
        }
    }
    if omitted_diagnostics > 0 {
        diagnostics.push(format!(
            "{omitted_diagnostics} additional X11 machine claim diagnostics were omitted"
        ));
    }
    MachineClaimCatalog {
        claims: loaded,
        diagnostics,
        complete,
    }
}

fn push_claim_diagnostic(diagnostics: &mut Vec<String>, omitted: &mut usize, message: String) {
    if diagnostics.len() < MAX_GRANT_DIAGNOSTICS.saturating_sub(1) {
        diagnostics.push(message);
    } else {
        *omitted += 1;
    }
}

fn push_grant_diagnostic(diagnostics: &mut Vec<String>, omitted: &mut usize, message: String) {
    if diagnostics.len() < MAX_GRANT_DIAGNOSTICS.saturating_sub(1) {
        diagnostics.push(message);
    } else {
        *omitted += 1;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SystemMachineRegistration {
    Present,
    Absent,
    Unknown(String),
}

/// Observe only the system machined registration directory. This helper
/// never consults the desktop user's runtime machine directory: claims on
/// this branch belong to system-scope nspawn machines.
fn system_machine_registration(machine: &str) -> SystemMachineRegistration {
    let path = crate::paths::runtime_machine_state(machine);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => SystemMachineRegistration::Present,
        Ok(_) => SystemMachineRegistration::Unknown(format!(
            "system machine registration is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            SystemMachineRegistration::Absent
        }
        Err(error) => SystemMachineRegistration::Unknown(format!(
            "cannot inspect system machine registration {}: {error}",
            path.display()
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MachineClaimObservation {
    Active,
    EndedCandidate,
    CleanupPending { since_unix_millis: u64 },
    CleanupReady,
    NeedsReview(String),
    Unknown(String),
}

fn observe_machine_claim(
    claim: &ManagedX11MachineClaim,
    current_boot_id: Option<&str>,
    registration: SystemMachineRegistration,
    now_unix_millis: u64,
) -> Option<MachineClaimObservation> {
    if !matches!(
        &claim.phase,
        MachineClaimPhase::Active | MachineClaimPhase::CleanupPending { .. }
    ) {
        return None;
    }
    let Some(current_boot_id) = current_boot_id else {
        return Some(MachineClaimObservation::Unknown(
            "host boot identity is unavailable".into(),
        ));
    };
    if claim.boot_id != current_boot_id {
        return Some(MachineClaimObservation::NeedsReview(
            "claim belongs to an earlier host boot".into(),
        ));
    }
    match (&claim.phase, registration) {
        (MachineClaimPhase::Active, SystemMachineRegistration::Present) => {
            Some(MachineClaimObservation::Active)
        }
        (MachineClaimPhase::Active, SystemMachineRegistration::Absent) => {
            Some(MachineClaimObservation::EndedCandidate)
        }
        (MachineClaimPhase::Active, SystemMachineRegistration::Unknown(reason)) => {
            Some(MachineClaimObservation::Unknown(reason))
        }
        (MachineClaimPhase::CleanupPending { .. }, SystemMachineRegistration::Present) => {
            Some(MachineClaimObservation::NeedsReview(
                "machine registration reappeared before cleanup; explicit preparation is required"
                    .into(),
            ))
        }
        (
            MachineClaimPhase::CleanupPending { since_unix_millis },
            SystemMachineRegistration::Absent,
        ) => Some(
            if now_unix_millis.saturating_sub(*since_unix_millis) >= CLAIM_RECONCILE_GRACE_MILLIS {
                MachineClaimObservation::CleanupReady
            } else {
                MachineClaimObservation::CleanupPending {
                    since_unix_millis: *since_unix_millis,
                }
            },
        ),
        (
            MachineClaimPhase::CleanupPending {
                since_unix_millis: _,
            },
            SystemMachineRegistration::Unknown(reason),
        ) => Some(MachineClaimObservation::Unknown(reason)),
        _ => None,
    }
}

fn proposed_claim_phase(
    claim: &ManagedX11MachineClaim,
    current_boot_id: Option<&str>,
    registration: SystemMachineRegistration,
    now_unix_millis: u64,
) -> Option<MachineClaimPhase> {
    match observe_machine_claim(claim, current_boot_id, registration, now_unix_millis)? {
        MachineClaimObservation::Active
        | MachineClaimObservation::CleanupPending { .. }
        | MachineClaimObservation::CleanupReady => None,
        MachineClaimObservation::EndedCandidate => Some(MachineClaimPhase::CleanupPending {
            since_unix_millis: now_unix_millis,
        }),
        MachineClaimObservation::NeedsReview(reason) => {
            Some(MachineClaimPhase::NeedsReview { reason })
        }
        MachineClaimObservation::Unknown(reason) => Some(MachineClaimPhase::Unknown { reason }),
    }
}

fn machine_claim_diagnostics(
    claims: &MachineClaimCatalog,
    current_boot_id: Option<&str>,
) -> Vec<String> {
    let mut diagnostics = claims.diagnostics.clone();
    let now = unix_millis().unwrap_or(0);
    for claim in &claims.claims {
        let registration = system_machine_registration(&claim.machine);
        let proposed = proposed_claim_phase(claim, current_boot_id, registration.clone(), now);
        let Some(observation) = observe_machine_claim(claim, current_boot_id, registration, now)
        else {
            continue;
        };
        let mut detail = match observation {
            MachineClaimObservation::Active => "system machine registration is present".to_owned(),
            MachineClaimObservation::EndedCandidate => {
                "system machine registration is absent; cleanup is only a candidate".to_owned()
            }
            MachineClaimObservation::CleanupPending { since_unix_millis } => {
                format!("cleanup pending since unix millisecond {since_unix_millis}")
            }
            MachineClaimObservation::CleanupReady => {
                "machine registration stayed absent; cleanup is ready for a guarded reconcile"
                    .to_owned()
            }
            MachineClaimObservation::NeedsReview(reason)
            | MachineClaimObservation::Unknown(reason) => reason,
        };
        if let Some(phase) = proposed {
            detail.push_str(match phase {
                MachineClaimPhase::CleanupPending { .. } => "; next state: cleanup pending",
                MachineClaimPhase::NeedsReview { .. } => "; next state: needs review",
                MachineClaimPhase::Unknown { .. } => "; next state: unknown",
                MachineClaimPhase::Active | MachineClaimPhase::Ended => "",
            });
        }
        if diagnostics.len() < MAX_GRANT_DIAGNOSTICS {
            diagnostics.push(format!(
                "X11 machine claim {} for {}: {detail}",
                claim.claim_id, claim.machine
            ));
        }
    }
    diagnostics
}

fn bounded_record_diagnostic(message: &str) -> String {
    const MAX_BYTES: usize = 512;
    let mut rendered = String::new();
    for character in message.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len().saturating_add(escaped.len()) > MAX_BYTES {
            rendered.push_str("...");
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}

fn desktop_observation(
    socket: &HostX11Socket,
    acl: X11AclSnapshot,
    state: Option<&X11RuntimeState>,
) -> X11DesktopObservation {
    let mut diagnostics = Vec::new();
    let host_boot_id = match host_boot_id() {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(format!("X11 server continuity: {error}"));
            None
        }
    };
    let server_peer_start_time = match x11_peer_start_time(socket) {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(format!("X11 server continuity: {error}"));
            None
        }
    };
    let records = state
        .map(X11RuntimeState::load_records)
        .unwrap_or_else(load_existing_grant_records);
    let claims = state
        .map(X11RuntimeState::load_claims)
        .unwrap_or_else(load_existing_machine_claims);
    diagnostics.extend(machine_claim_diagnostics(&claims, host_boot_id.as_deref()));
    X11DesktopObservation::new(
        acl,
        uzers::get_effective_uid(),
        host_boot_id,
        server_peer_start_time,
        records,
        diagnostics,
    )
}

fn ensure_access_sync(
    request: &X11AuthorizationRequest,
) -> Result<X11DesktopAuthorization, String> {
    let state = X11RuntimeState::open()?;
    let _lock = state.lock()?;
    let existing_claims = state.load_claims();
    if !existing_claims.complete {
        let detail = existing_claims
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the machine claim set is incomplete");
        return Err(format!(
            "X11 authorization was not attempted because Lasper cannot safely inspect its machine claims: {detail}"
        ));
    }
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
                desktop_observation(socket, before, Some(&state)),
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
            desktop_observation(socket, before, Some(&state)),
            X11AuthorizationDisposition::PreExisting,
        ));
    }

    let existing_records = state.load_records();
    if !existing_records.complete {
        let detail = existing_records
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the grant record set is incomplete");
        return Err(format!(
            "X11 authorization was not attempted because Lasper cannot safely inspect its existing grant records: {detail}"
        ));
    }
    if request.purpose() == X11AuthorizationPurpose::ExplicitSession {
        if let Some(record) = current_generation_record(
            &existing_records,
            request,
            server_peer_start_time,
            &host_boot_id()?,
        ) {
            let phase = match &record.phase {
                X11GrantRecordPhase::Pending => "pending",
                X11GrantRecordPhase::ConfirmedAdded => "confirmed but now absent",
                X11GrantRecordPhase::Revoked => "revoked",
                X11GrantRecordPhase::OutcomeUnknown { .. } => "outcome unknown",
            };
            return Err(format!(
                "X11 session authorization was not recreated because grant record {} for this machine instance and X server is {phase}; review the record and authorize access explicitly from Configure > Host Integration > X11",
                record.record_id
            ));
        }
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
        if let Err(claim_error) =
            state.write_claim(&ManagedX11MachineClaim::active_from_record(&record))
        {
            let reason = format!(
                "X11 access was confirmed, but its machine claim could not be persisted: {claim_error}"
            );
            record.phase = GrantRecordPhase::OutcomeUnknown {
                reason: reason.clone(),
            };
            let _ = state.write(&file_name, &record);
            return Err(reason);
        }
        return Ok(X11DesktopAuthorization::new(
            desktop_observation(socket, after, Some(&state)),
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

fn current_generation_record<'a>(
    catalog: &'a X11GrantRecordCatalog,
    request: &X11AuthorizationRequest,
    server_peer_start_time: u64,
    boot_id: &str,
) -> Option<&'a X11GrantRecordEvidence> {
    let projection = request.projection();
    let desired_entry = X11AclEntry::from_wire(
        Family::SERVER_INTERPRETED.into(),
        numeric_local_user_address(projection.identity().host_uid()),
    );
    let caller_uid = uzers::get_effective_uid();
    catalog.records.iter().find(|record| {
        record.target == *request.target()
            && record.display == projection.host_socket().display()
            && record.caller_uid == caller_uid
            && record.boot_id == boot_id
            && record.identity == projection.identity()
            && record.server_peer == projection.host_socket().peer_identity()
            && record.server_peer_start_time == server_peer_start_time
            && record.acl_entry == desired_entry
    })
}

fn revoke_access_sync(request: &X11RevokeRequest) -> Result<X11DesktopRevocation, String> {
    let state = X11RuntimeState::open_existing()?
        .ok_or_else(|| "X11 grant record storage does not exist".to_owned())?;
    let _lock = state.lock()?;
    let records = state.load_records();
    if !records.complete {
        let detail = records
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the grant record set is incomplete");
        return Err(format!(
            "X11 revocation was not attempted because Lasper cannot safely inspect its grant records: {detail}"
        ));
    }
    let claims = state.load_claims();
    if !claims.complete {
        let detail = claims
            .diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("the machine claim set is incomplete");
        return Err(format!(
            "X11 revocation was not attempted because Lasper cannot safely inspect its machine claims: {detail}"
        ));
    }
    let claim_is_active = claims.claims.iter().any(|claim| {
        claim.grant_record_id == request.record_id()
            && matches!(&claim.phase, MachineClaimPhase::Active)
    });

    let record_id = request.record_id();
    if !valid_record_id(record_id) {
        return Err("X11 revocation record ID is invalid".to_owned());
    }
    let file_name = format!("grant-{record_id}.json");
    let file = state
        .grants
        .read_bounded(&file_name, MAX_GRANT_RECORD_BYTES)
        .map_err(|error| format!("read X11 grant record {record_id}: {error}"))?
        .ok_or_else(|| format!("X11 grant record {record_id} does not exist"))?;
    if file.uid != uzers::get_effective_uid() || file.mode & 0o077 != 0 {
        return Err(format!(
            "X11 grant record {record_id} is not owned by the invoking user"
        ));
    }
    let mut record: ManagedX11GrantRecord = serde_json::from_slice(&file.bytes)
        .map_err(|error| format!("invalid X11 grant record {record_id}: {error}"))?;
    let evidence = record
        .clone()
        .into_evidence(&file_name)
        .map_err(|error| format!("invalid X11 grant record {record_id}: {error}"))?;
    if !matches!(evidence.phase, X11GrantRecordPhase::ConfirmedAdded) {
        return Err(format!(
            "X11 grant record {record_id} is not an active confirmed grant"
        ));
    }

    let projection = request.projection();
    let socket = projection.host_socket();
    let current_uid = uzers::get_effective_uid();
    if evidence.caller_uid != current_uid {
        return Err("X11 grant record belongs to another invoking user".to_owned());
    }
    if evidence.target != *request.target()
        || evidence.identity != projection.identity()
        || evidence.display != socket.display()
        || evidence.alternate_endpoint != socket.alternate()
        || evidence.source != socket.source()
        || evidence.canonical_source != socket.canonical_path()
        || evidence.socket_revision != socket.revision()
        || evidence.server_peer != socket.peer_identity()
    {
        return Err(
            "X11 grant record does not match the current machine instance or endpoint".to_owned(),
        );
    }
    let current_boot_id = host_boot_id()?;
    if evidence.boot_id != current_boot_id {
        return Err("X11 grant record belongs to another host boot".to_owned());
    }
    require_current_socket(socket, "before revoking access")?;
    let current_server_start = x11_peer_start_time(socket)?;
    if current_server_start != evidence.server_peer_start_time {
        return Err("X11 server generation changed since the grant was created".to_owned());
    }
    let (connection, _) = authenticated_connection(socket.source(), socket.display())?;
    let before = read_acl(&connection, socket.display())?;
    require_current_socket(socket, "before changing its ACL")?;
    match before.mode() {
        crate::application::x11::X11AccessControlMode::Disabled => {
            return Err("X11 access control is disabled; no ACL revoke was attempted".to_owned())
        }
        crate::application::x11::X11AccessControlMode::Unknown(mode) => {
            return Err(format!(
            "X server returned unsupported access-control mode {mode}; no ACL revoke was attempted"
        ))
        }
        crate::application::x11::X11AccessControlMode::Enabled => {}
    }

    if !before.contains(&evidence.acl_entry) {
        record.phase = GrantRecordPhase::Revoked;
        state.write(&file_name, &record).map_err(|error| {
            format!(
                "the exact ACL entry was already absent, but operation record {record_id} could not be finalized: {error}"
            )
        })?;
        if claim_is_active {
            state.end_claim(record_id).map_err(|error| {
                format!(
                    "the exact ACL entry was already absent and the grant record was finalized, but its machine claim could not be ended: {error}"
                )
            })?;
        }
        return Ok(X11DesktopRevocation::new(
            desktop_observation(socket, before, Some(&state)),
            X11RevocationDisposition::AlreadyAbsent {
                record_id: record_id.to_owned(),
            },
        ));
    }

    let (change_result, observed) =
        remove_and_observe(&connection, socket.display(), evidence.identity.host_uid());
    let socket_result = require_current_socket(socket, "while revoking access");
    let server_result = x11_peer_start_time(socket).and_then(|current| {
        (current == evidence.server_peer_start_time)
            .then_some(())
            .ok_or_else(|| "X11 server process changed while revoking access".to_owned())
    });
    let confirmed = change_result.is_ok()
        && socket_result.is_ok()
        && server_result.is_ok()
        && observed
            .as_ref()
            .is_ok_and(|snapshot| !snapshot.contains(&evidence.acl_entry));
    if confirmed {
        let after = observed.expect("confirmed observation is successful");
        record.phase = GrantRecordPhase::Revoked;
        state.write(&file_name, &record).map_err(|error| {
            format!(
                "X11 access was revoked, but operation record {record_id} could not be finalized ({error}); the confirmed record was preserved"
            )
        })?;
        if claim_is_active {
            state.end_claim(record_id).map_err(|error| {
                format!(
                    "X11 access was revoked and the grant record was finalized, but its machine claim could not be ended: {error}"
                )
            })?;
        }
        return Ok(X11DesktopRevocation::new(
            desktop_observation(socket, after, Some(&state)),
            X11RevocationDisposition::Revoked {
                record_id: record_id.to_owned(),
            },
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
        "the exact ACL entry was still present after the X server round trip".to_owned()
    } else {
        reason
    };
    record.phase = GrantRecordPhase::OutcomeUnknown {
        reason: reason.clone(),
    };
    let record_result = state.write(&file_name, &record);
    Err(match record_result {
        Ok(()) => format!(
            "X11 revocation outcome is unknown: {reason}; operation record {record_id} was preserved"
        ),
        Err(record_error) => format!(
            "X11 revocation outcome is unknown: {reason}; additionally, the operation record could not be updated: {record_error}"
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

fn remove_and_observe(
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
    fn source_observation_distinguishes_missing_invalid_and_unverified() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let source = directory.join("X0");
        assert_eq!(inspect_source(&source, directory), X11SourceState::Missing);

        fs::write(&source, "not a socket").unwrap();
        assert!(matches!(
            inspect_source(&source, directory),
            X11SourceState::Invalid(_)
        ));
        fs::remove_file(&source).unwrap();

        let _listener = std::os::unix::net::UnixListener::bind(&source).unwrap();
        assert!(matches!(
            inspect_source(&source, directory),
            X11SourceState::Unverified(_)
        ));
        assert!(matches!(
            inspect_source(directory, directory),
            X11SourceState::Unverified(_)
        ));
        assert!(matches!(
            inspect_source(Path::new("/outside/X0"), directory),
            X11SourceState::Unverified(_)
        ));

        let mut catalog = X11EndpointCatalog {
            sources: vec![X11SourceObservation {
                source: source.clone(),
                state: inspect_source(&source, directory),
            }],
            ..Default::default()
        };
        record_endpoint_failure(&mut catalog, &source, "X11 authentication failed");
        assert_eq!(
            catalog.sources[0].state,
            X11SourceState::Unverified("X11 authentication failed".into())
        );
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
            crate::application::sessions::X11FilesystemAccess::observed(true, true),
            identity,
        );
        let request = X11AuthorizationRequest::new(
            ShellTarget::new(
                crate::domain::machine::MachineName::new("archlinux").unwrap(),
                ValidatedGuestUserName::new("alice").unwrap(),
            ),
            projection,
        );
        let record_id = "0123456789abcdef0123456789abcdef";
        let record = ManagedX11GrantRecord::pending(&request, record_id.into(), 77).unwrap();
        let value = serde_json::to_value(&record).unwrap();
        let decoded: ManagedX11GrantRecord = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.record_id, record_id);
        assert_eq!(decoded.server_peer_start_time, 77);
        assert_eq!(decoded.acl_address, b"localuser\0#1437402088".to_vec());

        let claim = ManagedX11MachineClaim::active_from_record(&decoded);
        claim.validate(&format!("claim-{record_id}.json")).unwrap();
        assert_eq!(
            observe_machine_claim(
                &claim,
                Some(claim.boot_id.as_str()),
                SystemMachineRegistration::Present,
                1,
            ),
            Some(MachineClaimObservation::Active)
        );
        assert_eq!(
            observe_machine_claim(
                &claim,
                Some(claim.boot_id.as_str()),
                SystemMachineRegistration::Absent,
                1,
            ),
            Some(MachineClaimObservation::EndedCandidate)
        );
        assert_eq!(
            proposed_claim_phase(
                &claim,
                Some(claim.boot_id.as_str()),
                SystemMachineRegistration::Absent,
                123,
            ),
            Some(MachineClaimPhase::CleanupPending {
                since_unix_millis: 123
            })
        );
        let mut pending = claim.clone();
        pending.phase = MachineClaimPhase::CleanupPending {
            since_unix_millis: 100,
        };
        assert_eq!(
            observe_machine_claim(
                &pending,
                Some(pending.boot_id.as_str()),
                SystemMachineRegistration::Absent,
                100 + CLAIM_RECONCILE_GRACE_MILLIS - 1,
            ),
            Some(MachineClaimObservation::CleanupPending {
                since_unix_millis: 100
            })
        );
        assert_eq!(
            observe_machine_claim(
                &pending,
                Some(pending.boot_id.as_str()),
                SystemMachineRegistration::Absent,
                100 + CLAIM_RECONCILE_GRACE_MILLIS,
            ),
            Some(MachineClaimObservation::CleanupReady)
        );
        assert!(matches!(
            observe_machine_claim(
                &claim,
                Some("22222222-2222-4222-8222-222222222222"),
                SystemMachineRegistration::Present,
                1,
            ),
            Some(MachineClaimObservation::NeedsReview(_))
        ));
        assert!(matches!(
            observe_machine_claim(&claim, None, SystemMachineRegistration::Present, 1),
            Some(MachineClaimObservation::Unknown(_))
        ));
        let claim_value = serde_json::to_value(&claim).unwrap();
        let decoded_claim: ManagedX11MachineClaim =
            serde_json::from_value(claim_value.clone()).unwrap();
        assert_eq!(decoded_claim.claim_id, record_id);
        assert!(matches!(decoded_claim.phase, MachineClaimPhase::Active));
        let mut review_claim = decoded_claim.clone();
        review_claim.phase = MachineClaimPhase::NeedsReview {
            reason: "machine registration changed".into(),
        };
        review_claim
            .validate(&format!("claim-{record_id}.json"))
            .unwrap();
        let mut invalid_claim = claim_value;
        invalid_claim["key"]["acl_address"] = serde_json::json!([1, 2, 3]);
        let invalid_claim: ManagedX11MachineClaim = serde_json::from_value(invalid_claim).unwrap();
        assert!(invalid_claim
            .validate(&format!("claim-{record_id}.json"))
            .is_err());
        let invalid_claim: ManagedX11MachineClaim =
            serde_json::from_value(serde_json::to_value(&claim).unwrap()).unwrap();
        assert!(invalid_claim
            .validate("claim-ffffffffffffffffffffffffffffffff.json")
            .is_err());

        let mut revoked = decoded.clone();
        revoked.phase = GrantRecordPhase::Revoked;
        let revoked_value = serde_json::to_value(&revoked).unwrap();
        let decoded_revoked: ManagedX11GrantRecord = serde_json::from_value(revoked_value).unwrap();
        assert!(matches!(decoded_revoked.phase, GrantRecordPhase::Revoked));

        let mut unknown = value;
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ManagedX11GrantRecord>(unknown).is_err());

        let temporary = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = temporary.path().metadata().unwrap().uid();
        let grants = crate::adapters::trusted_state::TrustedDirectory::open_existing(
            temporary.path(),
            owner,
        )
        .unwrap()
        .open_or_create_child("grants", 0o700)
        .unwrap();
        let claims = crate::adapters::trusted_state::TrustedDirectory::open_existing(
            temporary.path(),
            owner,
        )
        .unwrap()
        .open_or_create_child("claims", 0o700)
        .unwrap();
        let file_name = format!("grant-{record_id}.json");
        grants
            .write_atomic(&file_name, &serde_json::to_vec(&record).unwrap(), 0o600)
            .unwrap();
        claims
            .write_atomic(
                &format!("claim-{record_id}.json"),
                &serde_json::to_vec(&claim).unwrap(),
                0o600,
            )
            .unwrap();
        let catalog = load_grant_records_from(&grants, owner);
        assert!(catalog.complete);
        assert!(catalog.diagnostics.is_empty());
        assert_eq!(catalog.records.len(), 1);
        assert_eq!(catalog.records[0].record_id, record_id);
        let claim_catalog = load_machine_claims_from(&claims, owner);
        assert!(claim_catalog.complete);
        assert!(claim_catalog.diagnostics.is_empty());
        assert_eq!(claim_catalog.claims.len(), 1);
        assert_eq!(claim_catalog.claims[0].grant_record_id, record_id);
        let explicit = X11AuthorizationRequest::for_explicit_session(
            request.target().clone(),
            request.projection().clone(),
        );
        assert_eq!(explicit.purpose(), X11AuthorizationPurpose::ExplicitSession);
        assert!(
            current_generation_record(&catalog, &explicit, 77, &catalog.records[0].boot_id,)
                .is_some()
        );
        assert!(current_generation_record(
            &catalog,
            &explicit,
            77,
            "22222222-2222-4222-8222-222222222222",
        )
        .is_none());

        grants
            .write_atomic(
                "grant-invalid.json",
                &serde_json::to_vec(&record).unwrap(),
                0o600,
            )
            .unwrap();
        grants
            .write_atomic(
                "grant-ffffffffffffffffffffffffffffffff.json",
                b"not-json",
                0o600,
            )
            .unwrap();
        let catalog = load_grant_records_from(&grants, owner);
        assert!(!catalog.complete);
        assert_eq!(catalog.records.len(), 1);
        assert!(catalog
            .diagnostics
            .iter()
            .any(|message| message.contains("invalid record JSON")));
        assert!(catalog
            .diagnostics
            .iter()
            .any(|message| message.contains("record ID does not match")));
    }
}
