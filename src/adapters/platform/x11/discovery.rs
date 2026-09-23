//! Observe and authenticate host X11 endpoint candidates.
//!
//! Discovery owns filesystem layout checks, display parsing, endpoint
//! identity snapshots, and source diagnostics. It does not mutate ACLs or
//! persist grant/lifecycle state.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use x11rb::connection::Connection;
use x11rb::reexports::x11rb_protocol::parse_display;

use crate::application::x11::{X11EndpointCatalog, X11SourceObservation, X11SourceState};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

use super::common::{MAX_DISCOVERED_DISPLAYS, MAX_INSPECTED_SOURCES, X11_SOCKET_DIRECTORY};
use super::transport::authenticated_connection;

pub(super) fn discover_sync(configured_sources: &[PathBuf]) -> X11EndpointCatalog {
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
    match validate_socket_directory(directory) {
        Ok(Some(warning)) => catalog.diagnostics.push(warning),
        Ok(None) => {}
        Err(reason) => {
            catalog.diagnostics.push(reason);
            return catalog;
        }
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

pub(super) fn inspect_source(source: &Path, directory: &Path) -> X11SourceState {
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

pub(super) fn record_endpoint_failure(
    catalog: &mut X11EndpointCatalog,
    source: &Path,
    reason: &str,
) {
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

pub(super) fn parse_local_display(value: &str) -> Option<u16> {
    let parsed = parse_display::parse_display_with_file_exists_callback(value, |_| false).ok()?;
    (parsed.host.is_empty() && matches!(parsed.protocol.as_deref(), None | Some("unix")))
        .then_some(parsed.display)
}

pub(super) fn parse_standard_socket_name(name: &str) -> Option<u16> {
    let number = name.strip_prefix('X')?;
    if number.is_empty()
        || number.ends_with('_')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

/// Validate the fixed X11 socket directory. A root-owned, non-sticky 0777
/// directory is accepted as a compatibility layout used by WSLg, but callers
/// receive a diagnostic because replacement races are less strongly bounded
/// than with the standard sticky `/tmp/.X11-unix` directory.
pub(super) fn validate_socket_directory(path: &Path) -> Result<Option<String>, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    let mode = metadata.permissions().mode();
    validate_socket_directory_metadata(path, metadata.is_dir(), metadata.uid(), mode)
}

pub(super) fn validate_socket_directory_metadata(
    path: &Path,
    is_directory: bool,
    owner_uid: u32,
    mode: u32,
) -> Result<Option<String>, String> {
    let mode = mode & 0o7777;
    if !is_directory {
        return Err(format!(
            "{} is not a directory (uid={owner_uid} mode={mode:04o})",
            path.display()
        ));
    }
    if owner_uid != 0 {
        return Err(format!(
            "{} is not root-owned (uid={owner_uid} mode={mode:04o})",
            path.display()
        ));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Ok(Some(format!(
            "{} is root-owned but writable by group/other without the sticky bit (mode={mode:04o}); accepting this compatibility layout with weaker replacement-race protection",
            path.display()
        )));
    }
    Ok(None)
}

pub(super) fn inspect_endpoint(
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
