//! Read-only inspection of a local X11 filesystem endpoint and its current ACL.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AccessControl, ConnectionExt, Family, Host};
use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
use x11rb::rust_connection::{DefaultStream, RustConnection};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const HELP: &str = "Usage: x11-inspect [--display DISPLAY] [--socket PATH]

Inspect a local X11 filesystem socket and read its current access list.
DISPLAY defaults to the current environment; supported forms: :N[.S], unix/:N[.S].
PATH defaults to /tmp/.X11-unix/XN and must be absolute.

Run as the desktop user. No access entries or container settings are changed.
An explicit socket is inspected as supplied; its relation to DISPLAY is unverified.
The query has a five-second deadline. Xauthority cookies are never printed.";

#[derive(Default)]
struct Options {
    display: Option<String>,
    socket: Option<PathBuf>,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Option<Self>> {
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => return Ok(None),
                Some("--display") if options.display.is_none() => {
                    options.display = Some(
                        args.next()
                            .context("--display requires a value")?
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("DISPLAY must be valid UTF-8"))?,
                    );
                }
                Some("--socket") if options.socket.is_none() => {
                    options.socket = Some(args.next().context("--socket requires a path")?.into());
                }
                _ => bail!("unknown or repeated option: {}", arg.to_string_lossy()),
            }
        }
        Ok(Some(options))
    }
}

#[derive(Debug, Serialize)]
struct DisplaySelection {
    requested: String,
    number: u16,
    screen: u16,
}

#[derive(Debug)]
struct Target {
    display: DisplaySelection,
    socket: PathBuf,
}

impl Target {
    fn parse(display: &str, socket: Option<PathBuf>) -> Result<Self> {
        let parsed = parse_display::parse_display_with_file_exists_callback(display, |_| false)
            .context("invalid DISPLAY")?;
        if !parsed.host.is_empty() || !matches!(parsed.protocol.as_deref(), None | Some("unix")) {
            bail!("only local DISPLAY forms :N[.S] and unix/:N[.S] are supported");
        }
        let socket =
            socket.unwrap_or_else(|| PathBuf::from(format!("/tmp/.X11-unix/X{}", parsed.display)));
        if !socket.is_absolute() {
            bail!("--socket must be an absolute filesystem path");
        }
        Ok(Self {
            display: DisplaySelection {
                requested: display.to_owned(),
                number: parsed.display,
                screen: parsed.screen,
            },
            socket,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SocketObservation {
    requested_path: PathBuf,
    canonical_path: PathBuf,
    device: u64,
    inode: u64,
    owner_uid: u32,
    owner_gid: u32,
    mode: String,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
}

impl SocketObservation {
    fn read(path: &Path) -> Result<Self> {
        let canonical_path =
            fs::canonicalize(path).with_context(|| format!("resolve socket {}", path.display()))?;
        let metadata = fs::metadata(&canonical_path).context("stat socket")?;
        if !metadata.file_type().is_socket() {
            bail!("{} is not a Unix socket", path.display());
        }
        Ok(Self {
            requested_path: path.to_owned(),
            canonical_path,
            device: metadata.dev(),
            inode: metadata.ino(),
            owner_uid: metadata.uid(),
            owner_gid: metadata.gid(),
            mode: format!("{:o}", metadata.mode() & 0o7777),
            ctime_seconds: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        })
    }

    fn recheck(&self) -> Result<()> {
        if Self::read(&self.requested_path)? != *self {
            bail!("socket path or metadata changed during inspection; retry the query");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct ServerInterpreted {
    type_name: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Serialize)]
struct AclEntry {
    family: u8,
    address_hex: String,
    server_interpreted: Option<ServerInterpreted>,
}

impl From<Host> for AclEntry {
    fn from(host: Host) -> Self {
        let server_interpreted = (host.family == Family::SERVER_INTERPRETED)
            .then(|| host.address.iter().position(|byte| *byte == 0))
            .flatten()
            .map(|separator| ServerInterpreted {
                type_name: String::from_utf8(host.address[..separator].to_vec()).ok(),
                value: String::from_utf8(host.address[separator + 1..].to_vec()).ok(),
            });
        Self {
            family: host.family.into(),
            address_hex: host
                .address
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            server_interpreted,
        }
    }
}

#[derive(Serialize)]
struct ServerSetup {
    protocol_major: u16,
    protocol_minor: u16,
    vendor: String,
    release_number: u32,
    screen_count: usize,
}

#[derive(Serialize)]
struct Inspection {
    schema: &'static str,
    display: DisplaySelection,
    socket: SocketObservation,
    caller_euid: u32,
    authority_entry_supplied: bool,
    authority_lookup_error: Option<String>,
    server_setup: ServerSetup,
    access_control_mode: u8,
    access_control_enabled: Option<bool>,
    access_entries: Vec<AclEntry>,
}

type AuthorityEntry = Option<(Vec<u8>, Vec<u8>)>;

fn inspect(
    target: Target,
    lookup_auth: impl FnOnce(xauth::Family, &[u8], u16) -> io::Result<AuthorityEntry>,
) -> Result<Inspection> {
    let socket = SocketObservation::read(&target.socket)?;
    // Connect to this exact filesystem endpoint. The library's generic connect
    // path may prefer an abstract socket or fall back to TCP for a local DISPLAY.
    let stream = UnixStream::connect(&target.socket)
        .with_context(|| format!("connect to {}", target.socket.display()))?;
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)?;
    let (authority, authority_lookup_error) =
        match lookup_auth(family, &address, target.display.number) {
            Ok(authority) => (authority, None),
            Err(error) => (None, Some(error.to_string())),
        };
    let authority_entry_supplied = authority.is_some();
    let (auth_name, auth_data) = authority.unwrap_or_default();
    let connection = RustConnection::connect_to_stream_with_auth_info(
        stream,
        target.display.screen.into(),
        auth_name,
        auth_data,
    )
    .context("X11 connection setup failed")?;
    let setup = connection.setup();
    let server_setup = ServerSetup {
        protocol_major: setup.protocol_major_version,
        protocol_minor: setup.protocol_minor_version,
        vendor: String::from_utf8_lossy(&setup.vendor).into_owned(),
        release_number: setup.release_number,
        screen_count: setup.roots.len(),
    };
    let hosts = connection
        .list_hosts()
        .context("send X11 ListHosts")?
        .reply()
        .context("read X11 ListHosts reply")?;
    socket.recheck()?;
    Ok(Inspection {
        schema: "LASPER_X11_INSPECTION_V1",
        display: target.display,
        socket,
        caller_euid: uzers::get_effective_uid(),
        authority_entry_supplied,
        authority_lookup_error,
        server_setup,
        access_control_mode: hosts.mode.into(),
        access_control_enabled: match hosts.mode {
            AccessControl::ENABLE => Some(true),
            AccessControl::DISABLE => Some(false),
            _ => None,
        },
        access_entries: hosts.hosts.into_iter().map(AclEntry::from).collect(),
    })
}

fn run() -> Result<()> {
    let Some(options) = Options::parse(env::args_os().skip(1))? else {
        println!("{HELP}");
        return Ok(());
    };
    let display = options
        .display
        .map(Ok)
        .unwrap_or_else(|| env::var("DISPLAY"))
        .context("set DISPLAY or pass --display")?;
    let target = Target::parse(&display, options.socket)?;
    // This standalone process exits on timeout, including if X11 setup stalls.
    // This is not a worker lifetime strategy for the long-running Lasper TUI.
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(inspect(target, xauth::get_auth));
    });
    let report = match receiver.recv_timeout(QUERY_TIMEOUT) {
        Ok(report) => report?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("X11 inspection timed out after five seconds")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("X11 inspection worker stopped unexpectedly")
        }
    };
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("x11-inspect: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use x11rb::protocol::xproto::{ListHostsReply, Screen, Setup};
    use x11rb::x11_utils::Serialize as WireSerialize;

    #[test]
    fn local_display_keeps_screen_separate_from_socket() {
        for display in [":12.3", "unix/:12.3"] {
            let target = Target::parse(display, None).unwrap();
            assert_eq!(target.display.number, 12);
            assert_eq!(target.display.screen, 3);
            assert_eq!(target.socket, Path::new("/tmp/.X11-unix/X12"));
        }
        assert_eq!(Target::parse(":0", None).unwrap().display.screen, 0);
        assert_eq!(
            Target::parse(":65535", None).unwrap().socket,
            Path::new("/tmp/.X11-unix/X65535")
        );
    }

    #[test]
    fn remote_or_invalid_display_cannot_be_made_local_by_socket_override() {
        for display in ["host:0", "localhost:0", "tcp/:0", "", ":", ":0.x", ":65536"] {
            assert!(
                Target::parse(display, Some("/tmp/X0".into())).is_err(),
                "accepted {display:?}"
            );
        }
        assert!(Target::parse(":0", Some("relative/X0".into())).is_err());
    }

    #[test]
    fn acl_preserves_numeric_users_names_and_unknown_bytes() {
        for value in ["#1000", "Lumie"] {
            let address = format!("localuser\0{value}").into_bytes();
            let entry = AclEntry::from(Host {
                family: Family::SERVER_INTERPRETED,
                address,
            });
            let si = entry.server_interpreted.unwrap();
            assert_eq!(si.type_name.as_deref(), Some("localuser"));
            assert_eq!(si.value.as_deref(), Some(value));
        }
        let unknown = AclEntry::from(Host {
            family: 250.into(),
            address: vec![0, 255, 0, 128],
        });
        assert_eq!(unknown.family, 250);
        assert_eq!(unknown.address_hex, "00ff0080");
        assert!(unknown.server_interpreted.is_none());
        let malformed = AclEntry::from(Host {
            family: Family::SERVER_INTERPRETED,
            address: b"localuser".to_vec(),
        });
        assert!(malformed.server_interpreted.is_none());
        let non_utf8 = AclEntry::from(Host {
            family: Family::SERVER_INTERPRETED,
            address: vec![b'x', 0, 255],
        });
        assert_eq!(non_utf8.address_hex, "7800ff");
        assert!(non_utf8.server_interpreted.unwrap().value.is_none());
    }

    #[test]
    fn socket_observation_rejects_regular_files_and_detects_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("X0_");
        fs::write(&path, []).unwrap();
        assert!(SocketObservation::read(&path).is_err());
        fs::remove_file(&path).unwrap();
        let _first = UnixListener::bind(&path).unwrap();
        let observation = SocketObservation::read(&path).unwrap();
        observation.recheck().unwrap();
        fs::rename(&path, temp.path().join("old-socket")).unwrap();
        let _replacement = UnixListener::bind(&path).unwrap();
        assert!(observation.recheck().is_err());
        fs::remove_file(&path).unwrap();
        assert!(observation.recheck().is_err());
    }

    #[test]
    fn socket_observation_detects_symlink_retargeting() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selected");
        let first = temp.path().join("X0");
        let second = temp.path().join("X0_");
        let _first = UnixListener::bind(&first).unwrap();
        let _second = UnixListener::bind(&second).unwrap();
        symlink(&first, &path).unwrap();
        let observation = SocketObservation::read(&path).unwrap();
        assert_eq!(observation.canonical_path, first);
        fs::remove_file(&path).unwrap();
        symlink(&second, &path).unwrap();
        assert!(observation.recheck().is_err());
    }

    #[test]
    fn filesystem_connection_reads_acl_without_changing_access() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("X0_");
        let listener = UnixListener::bind(&path).unwrap();
        let target = Target::parse(":0", Some(path.clone())).unwrap();
        // Bound accept as well as the protocol reads so a client regression
        // cannot leave the fixture waiting indefinitely.
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no client connection");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 12];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(u16::from_ne_bytes([request[2], request[3]]), 11);
            assert_eq!(&request[6..10], &[0; 4]); // No cookie in this fixture.
            let mut setup = Setup {
                status: 1,
                protocol_major_version: 11,
                resource_id_base: 0x0100_0000,
                resource_id_mask: 0x00ff_ffff,
                maximum_request_length: u16::MAX,
                min_keycode: 8,
                max_keycode: 255,
                vendor: b"Lasper test server".to_vec(),
                roots: vec![Screen::default()],
                ..Default::default()
            };
            setup.length = ((WireSerialize::serialize(&setup).len() - 8) / 4) as u16;
            stream.write_all(&WireSerialize::serialize(&setup)).unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request[0], 110); // ListHosts, not ChangeHosts (109).
            assert_eq!(u16::from_ne_bytes([request[2], request[3]]), 1);
            let mut reply = ListHostsReply {
                mode: AccessControl::ENABLE,
                sequence: 1,
                hosts: vec![Host {
                    family: Family::SERVER_INTERPRETED,
                    address: b"localuser\0#1437402088".to_vec(),
                }],
                ..Default::default()
            };
            reply.length = ((WireSerialize::serialize(&reply).len() - 32) / 4) as u32;
            stream.write_all(&WireSerialize::serialize(&reply)).unwrap();
            assert_eq!(stream.read(&mut request).unwrap(), 0); // No further requests.
        });
        let report = inspect(target, |_, _, _| Ok(None)).unwrap();
        server.join().unwrap();
        assert_eq!(report.socket.requested_path, path);
        assert!(!report.authority_entry_supplied);
        assert_eq!(report.access_control_enabled, Some(true));
        assert_eq!(report.access_entries.len(), 1);
        assert_eq!(
            report.access_entries[0]
                .server_interpreted
                .as_ref()
                .unwrap()
                .value
                .as_deref(),
            Some("#1437402088")
        );
    }
}
