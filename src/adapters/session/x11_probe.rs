//! Fixed guest-side observation used by the X11 projection resolver.
//!
//! The probe reports only the selected account, its user namespace, and two
//! socket paths. Host UID mapping and X server ACL inspection stay outside the
//! guest and are never accepted from probe output.

use crate::application::sessions::{
    ObservedGuestIdentity, ObservedNamespaceIdentity, SessionError, TerminalSessionHandle,
    ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use std::path::{Component, Path};
use std::time::Duration;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT_BYTES: usize = 12 * 1024;
const MAX_PROBE_LINE_BYTES: usize = 1024;
const MAX_TARGET_PATH_BYTES: usize = 4096;
const PROBE_MAGIC: &[u8] = b"LASPER_X11_PROJECTION_PROBE_V1";

// Linux pathname AF_UNIX connect requires write access to the socket, not
// read/execute bits or mode 0666. This does not test an abstract socket or an
// X11 setup handshake. Keep that distinction when reporting probe results.
const X11_PROBE_SCRIPT: &str = r#"euid=
egid=
while read -r key _real effective _rest; do
    case "$key" in
        Uid:) euid=$effective ;;
        Gid:) egid=$effective ;;
    esac
done < /proc/self/status
userns_stat=$(LC_ALL=C stat -Lc '%d:%i' -- /proc/self/ns/user) || exit 1
userns_device=${userns_stat%%:*}
userns_inode=${userns_stat#*:}
socket_state() {
    if [ ! -e "$1" ]; then
        printf '%s' MISSING
    elif [ ! -S "$1" ]; then
        printf '%s' NOT_SOCKET
    elif [ ! -w "$1" ]; then
        printf '%s' DENIED
    else
        printf '%s' ACCESSIBLE
    fi
}
mount_state=$(socket_state "$1")
client_state=$(socket_state "$2")
mount_stat=
client_stat=
if [ "$mount_state" = ACCESSIBLE ] || [ "$mount_state" = DENIED ]; then
    mount_stat=$(LC_ALL=C stat -Lc '%d:%i' -- "$1") || exit 1
fi
if [ "$client_state" = ACCESSIBLE ] || [ "$client_state" = DENIED ]; then
    client_stat=$(LC_ALL=C stat -Lc '%d:%i' -- "$2") || exit 1
fi
printf '%s\n' \
    'LASPER_X11_PROJECTION_PROBE_V1' \
    "EUID=$euid" \
    "EGID=$egid" \
    "USERNS_DEVICE=$userns_device" \
    "USERNS_INODE=$userns_inode" \
    "MOUNT=$mount_state"
if [ -n "$mount_stat" ]; then
    printf '%s\n' \
        "MOUNT_DEVICE=${mount_stat%%:*}" \
        "MOUNT_INODE=${mount_stat#*:}"
fi
printf '%s\n' "CLIENT=$client_state"
if [ -n "$client_stat" ]; then
    printf '%s\n' \
        "CLIENT_DEVICE=${client_stat%%:*}" \
        "CLIENT_INODE=${client_stat#*:}"
fi
printf '%s\n' 'RESULT=READY'
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11ProjectionProbeRequest {
    machine: MachineName,
    user: ValidatedGuestUserName,
    mount_target: String,
    client_path: String,
}

impl X11ProjectionProbeRequest {
    pub(crate) fn new(
        machine: MachineName,
        user: ValidatedGuestUserName,
        mount_target: &Path,
        client_path: &Path,
    ) -> Result<Self, X11ProjectionProbeRequestError> {
        Ok(Self {
            machine,
            user,
            mount_target: validate_absolute_path(mount_target)?,
            client_path: validate_absolute_path(client_path)?,
        })
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub(crate) fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }

    pub(crate) fn path(&self) -> &'static str {
        "/bin/sh"
    }

    pub(crate) fn args(&self) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            X11_PROBE_SCRIPT.into(),
            "lasper-x11-projection-probe".into(),
            self.mount_target.clone(),
            self.client_path.clone(),
        ]
    }
}

fn validate_absolute_path(path: &Path) -> Result<String, X11ProjectionProbeRequestError> {
    if !path.is_absolute() {
        return Err(X11ProjectionProbeRequestError::NotAbsolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(X11ProjectionProbeRequestError::RelativeComponent);
    }
    let value = path
        .to_str()
        .ok_or(X11ProjectionProbeRequestError::NonUtf8)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(X11ProjectionProbeRequestError::InvalidValue);
    }
    if value.len() > MAX_TARGET_PATH_BYTES {
        return Err(X11ProjectionProbeRequestError::TooLong {
            maximum: MAX_TARGET_PATH_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum X11ProjectionProbeRequestError {
    #[error("X11 probe path must be absolute")]
    NotAbsolute,
    #[error("X11 probe path must not contain relative path components")]
    RelativeComponent,
    #[error("X11 probe path is not valid UTF-8")]
    NonUtf8,
    #[error("X11 probe path contains an empty or control-character value")]
    InvalidValue,
    #[error("X11 probe path exceeds the {maximum}-byte path limit")]
    TooLong { maximum: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum X11SocketAccess {
    Accessible,
    Missing,
    Denied,
    NotSocket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct X11SocketObservation {
    pub(crate) access: X11SocketAccess,
    pub(crate) identity: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct X11ProjectionProbeObservation {
    pub(crate) identity: ObservedGuestIdentity,
    pub(crate) user_namespace: ObservedNamespaceIdentity,
    pub(crate) mount: X11SocketObservation,
    pub(crate) client: X11SocketObservation,
}

pub(crate) async fn collect_x11_probe(
    handle: &mut TerminalSessionHandle,
) -> Result<X11ProjectionProbeObservation, SessionError> {
    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("X11 projection probe output is unavailable"))?;
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut bytes = Vec::new();

    loop {
        match tokio::time::timeout_at(deadline, output.recv()).await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_PROBE_OUTPUT_BYTES {
                    handle.close();
                    return Err(SessionError::new(
                        "X11 projection probe output exceeded its limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
                if let Some(frame_end) = complete_probe_frame_end(&bytes) {
                    handle.close();
                    return parse_x11_probe(&bytes[..frame_end]);
                }
            }
            Ok(None) => break,
            Err(_) => {
                handle.close();
                return Err(SessionError::new("X11 projection probe timed out"));
            }
        }
    }
    handle.close();
    parse_x11_probe(&bytes)
}

fn complete_probe_frame_end(bytes: &[u8]) -> Option<usize> {
    let marker_offset = bytes
        .windows(PROBE_MAGIC.len())
        .rposition(|window| window == PROBE_MAGIC)?;
    let mut offset = marker_offset + PROBE_MAGIC.len();
    if bytes[offset..].starts_with(b"\r\n") {
        offset += 2;
    } else if bytes[offset..].starts_with(b"\n") {
        offset += 1;
    } else {
        return None;
    }
    while let Some(line_length) = bytes[offset..].iter().position(|byte| *byte == b'\n') {
        let frame_end = offset + line_length + 1;
        let line = bytes[offset..frame_end - 1]
            .strip_suffix(b"\r")
            .unwrap_or(&bytes[offset..frame_end - 1]);
        if line == b"RESULT=READY" {
            return Some(frame_end);
        }
        offset = frame_end;
    }
    None
}

#[derive(Default)]
struct ProbeFields {
    uid: Option<u32>,
    gid: Option<u32>,
    userns_device: Option<u64>,
    userns_inode: Option<u64>,
    mount_access: Option<X11SocketAccess>,
    mount_device: Option<u64>,
    mount_inode: Option<u64>,
    client_access: Option<X11SocketAccess>,
    client_device: Option<u64>,
    client_inode: Option<u64>,
    ready: bool,
}

fn parse_x11_probe(bytes: &[u8]) -> Result<X11ProjectionProbeObservation, SessionError> {
    if bytes.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(SessionError::new(
            "X11 projection probe output exceeded its limit",
        ));
    }
    let Some(marker_offset) = bytes
        .windows(PROBE_MAGIC.len())
        .rposition(|window| window == PROBE_MAGIC)
    else {
        return Err(incomplete_probe_error("protocol marker is missing", bytes));
    };
    let framed = &bytes[marker_offset + PROBE_MAGIC.len()..];
    let Some(framed) = framed
        .strip_prefix(b"\r\n")
        .or_else(|| framed.strip_prefix(b"\n"))
    else {
        return Err(incomplete_probe_error(
            "protocol marker is not followed by a line ending",
            bytes,
        ));
    };

    let mut fields = ProbeFields::default();
    for raw_line in framed.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.len() > MAX_PROBE_LINE_BYTES {
            return Err(SessionError::new(
                "X11 projection probe line exceeded its limit",
            ));
        }
        if line.is_empty() {
            continue;
        }
        if fields.ready {
            return Err(SessionError::new(
                "X11 projection probe emitted data after its result",
            ));
        }
        if line == b"RESULT=READY" {
            fields.ready = true;
            continue;
        }
        let Some(separator) = line.iter().position(|byte| *byte == b'=') else {
            return Err(SessionError::new(
                "X11 projection probe returned a malformed field",
            ));
        };
        let (key, value) = (&line[..separator], &line[separator + 1..]);
        match key {
            b"EUID" => set_once(&mut fields.uid, parse_u32(value, "EUID")?, "EUID")?,
            b"EGID" => set_once(&mut fields.gid, parse_u32(value, "EGID")?, "EGID")?,
            b"USERNS_DEVICE" => set_once(
                &mut fields.userns_device,
                parse_u64(value, "USERNS_DEVICE")?,
                "USERNS_DEVICE",
            )?,
            b"USERNS_INODE" => set_once(
                &mut fields.userns_inode,
                parse_u64(value, "USERNS_INODE")?,
                "USERNS_INODE",
            )?,
            b"MOUNT" => set_once(
                &mut fields.mount_access,
                parse_access(value, "MOUNT")?,
                "MOUNT",
            )?,
            b"MOUNT_DEVICE" => set_once(
                &mut fields.mount_device,
                parse_u64(value, "MOUNT_DEVICE")?,
                "MOUNT_DEVICE",
            )?,
            b"MOUNT_INODE" => set_once(
                &mut fields.mount_inode,
                parse_u64(value, "MOUNT_INODE")?,
                "MOUNT_INODE",
            )?,
            b"CLIENT" => set_once(
                &mut fields.client_access,
                parse_access(value, "CLIENT")?,
                "CLIENT",
            )?,
            b"CLIENT_DEVICE" => set_once(
                &mut fields.client_device,
                parse_u64(value, "CLIENT_DEVICE")?,
                "CLIENT_DEVICE",
            )?,
            b"CLIENT_INODE" => set_once(
                &mut fields.client_inode,
                parse_u64(value, "CLIENT_INODE")?,
                "CLIENT_INODE",
            )?,
            _ => {
                return Err(SessionError::new(format!(
                    "X11 projection probe returned an unknown field: {}",
                    escaped_field_key(key)
                )))
            }
        }
    }

    let identity = ObservedGuestIdentity::new(
        required(fields.uid, "EUID", bytes)?,
        required(fields.gid, "EGID", bytes)?,
    );
    let user_namespace = ObservedNamespaceIdentity::new(
        required(fields.userns_device, "USERNS_DEVICE", bytes)?,
        required(fields.userns_inode, "USERNS_INODE", bytes)?,
    );
    let mount = socket_observation(
        required(fields.mount_access, "MOUNT", bytes)?,
        fields.mount_device,
        fields.mount_inode,
        "MOUNT",
        bytes,
    )?;
    let client = socket_observation(
        required(fields.client_access, "CLIENT", bytes)?,
        fields.client_device,
        fields.client_inode,
        "CLIENT",
        bytes,
    )?;
    if !fields.ready {
        return Err(incomplete_probe_error("missing RESULT", bytes));
    }
    Ok(X11ProjectionProbeObservation {
        identity,
        user_namespace,
        mount,
        client,
    })
}

fn required<T>(value: Option<T>, field: &str, bytes: &[u8]) -> Result<T, SessionError> {
    value.ok_or_else(|| incomplete_probe_error(&format!("missing {field}"), bytes))
}

fn socket_observation(
    access: X11SocketAccess,
    device: Option<u64>,
    inode: Option<u64>,
    field: &str,
    bytes: &[u8],
) -> Result<X11SocketObservation, SessionError> {
    let identity = match (access, device, inode) {
        (X11SocketAccess::Accessible | X11SocketAccess::Denied, Some(device), Some(inode)) => {
            Some((device, inode))
        }
        (X11SocketAccess::Accessible | X11SocketAccess::Denied, _, _) => {
            return Err(incomplete_probe_error(
                &format!("{field} socket is missing DEVICE or INODE"),
                bytes,
            ));
        }
        (_, None, None) => None,
        _ => {
            return Err(SessionError::new(format!(
                "X11 projection probe returned {field} identity for a non-socket"
            )));
        }
    };
    Ok(X11SocketObservation { access, identity })
}

fn set_once<T>(slot: &mut Option<T>, value: T, field: &str) -> Result<(), SessionError> {
    if slot.replace(value).is_some() {
        return Err(SessionError::new(format!(
            "X11 projection probe repeated {field}"
        )));
    }
    Ok(())
}

fn parse_access(value: &[u8], field: &str) -> Result<X11SocketAccess, SessionError> {
    match value {
        b"ACCESSIBLE" => Ok(X11SocketAccess::Accessible),
        b"MISSING" => Ok(X11SocketAccess::Missing),
        b"DENIED" => Ok(X11SocketAccess::Denied),
        b"NOT_SOCKET" => Ok(X11SocketAccess::NotSocket),
        _ => Err(SessionError::new(format!(
            "X11 projection probe returned an invalid {field}"
        ))),
    }
}

fn parse_u32(value: &[u8], field: &str) -> Result<u32, SessionError> {
    u32::try_from(parse_u64(value, field)?)
        .map_err(|_| SessionError::new(format!("X11 projection probe returned an invalid {field}")))
}

fn parse_u64(value: &[u8], field: &str) -> Result<u64, SessionError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(SessionError::new(format!(
            "X11 projection probe returned an invalid {field}"
        )));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            SessionError::new(format!("X11 projection probe returned an invalid {field}"))
        })
}

fn incomplete_probe_error(reason: &str, bytes: &[u8]) -> SessionError {
    SessionError::new(format!(
        "X11 projection probe result was incomplete ({reason}; captured output: {})",
        escaped_output_tail(bytes)
    ))
}

fn escaped_output_tail(bytes: &[u8]) -> String {
    const PREVIEW_BYTES: usize = 512;
    if bytes.is_empty() {
        return "<empty>".into();
    }
    let truncated = bytes.len() > PREVIEW_BYTES;
    let start = bytes.len().saturating_sub(PREVIEW_BYTES);
    let mut preview = String::with_capacity(bytes.len().min(PREVIEW_BYTES));
    if truncated {
        preview.push_str("...");
    }
    for byte in &bytes[start..] {
        preview.extend(std::ascii::escape_default(*byte).map(char::from));
    }
    preview
}

fn escaped_field_key(bytes: &[u8]) -> String {
    const PREVIEW_BYTES: usize = 96;
    let mut preview = String::new();
    for byte in bytes.iter().take(PREVIEW_BYTES) {
        preview.extend(std::ascii::escape_default(*byte).map(char::from));
    }
    if bytes.len() > PREVIEW_BYTES {
        preview.push_str("...");
    }
    if preview.is_empty() {
        "<empty>".into()
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use std::os::unix::fs::MetadataExt;

    fn machine() -> MachineName {
        MachineName::new("demo").unwrap()
    }

    fn user() -> ValidatedGuestUserName {
        ValidatedGuestUserName::new("alice").unwrap()
    }

    #[test]
    fn fixed_script_reports_mount_client_and_namespace_identity() {
        let runtime = tempfile::tempdir().unwrap();
        let mount = runtime.path().join("mount.sock");
        let client = runtime.path().join("client.sock");
        let _mount_listener = std::os::unix::net::UnixListener::bind(&mount).unwrap();
        std::os::unix::fs::symlink(&mount, &client).unwrap();
        let request = X11ProjectionProbeRequest::new(machine(), user(), &mount, &client).unwrap();
        let output = std::process::Command::new(request.path())
            .args(&request.args()[1..])
            .output()
            .unwrap();
        assert!(output.status.success());
        let observation = parse_x11_probe(&output.stdout).unwrap();
        let metadata = std::fs::metadata(&mount).unwrap();
        let userns = std::fs::metadata("/proc/self/ns/user").unwrap();
        assert_eq!(observation.mount.access, X11SocketAccess::Accessible);
        assert_eq!(observation.client.access, X11SocketAccess::Accessible);
        assert_eq!(
            observation.mount.identity,
            Some((metadata.dev(), metadata.ino()))
        );
        assert_eq!(observation.client.identity, observation.mount.identity);
        assert_eq!(
            observation.user_namespace,
            ObservedNamespaceIdentity::new(userns.dev(), userns.ino())
        );
    }

    #[test]
    fn pathname_probe_agrees_with_kernel_connect_permissions() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};

        // Root can bypass DAC; run the permission comparison as an ordinary
        // user. The actual uid-mapped guest check uses that guest's identity.
        if uzers::get_effective_uid() == 0 {
            return;
        }
        let runtime = tempfile::tempdir().unwrap();
        let socket = runtime.path().join("X0");
        let _listener = UnixListener::bind(&socket).unwrap();
        let request = X11ProjectionProbeRequest::new(machine(), user(), &socket, &socket).unwrap();
        for (mode, expected) in [
            (0o100, X11SocketAccess::Denied),
            (0o500, X11SocketAccess::Denied),
            (0o200, X11SocketAccess::Accessible),
            (0o600, X11SocketAccess::Accessible),
        ] {
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(mode)).unwrap();
            let output = std::process::Command::new(request.path())
                .args(&request.args()[1..])
                .output()
                .unwrap();
            assert!(output.status.success());
            let observation = parse_x11_probe(&output.stdout).unwrap();
            assert_eq!(observation.mount.access, expected, "mode {mode:04o}");
            assert_eq!(observation.client.access, expected, "mode {mode:04o}");
            match UnixStream::connect(&socket) {
                Ok(_) => assert_eq!(expected, X11SocketAccess::Accessible),
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                    assert_eq!(expected, X11SocketAccess::Denied);
                }
            }
        }
    }

    #[tokio::test]
    async fn collection_returns_as_soon_as_the_complete_frame_arrives() {
        let (mut handle, endpoint) = crate::application::sessions::terminal_session_channel(
            SessionId::new(1).unwrap(),
            TerminalAttachmentKind::Login,
        );
        endpoint
            .output
            .send(b"LASPER_X11_PROJECTION_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nUSERNS_DEVICE=4\r\nUSERNS_INODE=5\r\nMOUNT=MISSING\r\nCLIENT=MISSING\r\nRESULT=READY\r\n".to_vec())
            .await
            .unwrap();
        let observation =
            tokio::time::timeout(Duration::from_millis(100), collect_x11_probe(&mut handle))
                .await
                .expect("complete X11 probe waited for the PTY to close")
                .unwrap();
        assert_eq!(observation.mount.access, X11SocketAccess::Missing);
    }

    #[test]
    fn parser_rejects_missing_duplicate_and_inconsistent_fields() {
        let prefix = "LASPER_X11_PROJECTION_PROBE_V1\nEUID=1000\nEGID=1000\nUSERNS_DEVICE=4\nUSERNS_INODE=5\n";
        assert!(parse_x11_probe(
            format!("{prefix}MOUNT=ACCESSIBLE\nMOUNT_DEVICE=1\nCLIENT=MISSING\nRESULT=READY\n")
                .as_bytes()
        )
        .is_err());
        assert!(parse_x11_probe(
            format!("{prefix}MOUNT=MISSING\nCLIENT=MISSING\nCLIENT=MISSING\nRESULT=READY\n")
                .as_bytes()
        )
        .is_err());
        assert!(parse_x11_probe(
            format!("{prefix}MOUNT=MISSING\nMOUNT_DEVICE=1\nCLIENT=MISSING\nRESULT=READY\n")
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn parser_uses_the_last_frame_after_assignment_like_terminal_noise() {
        let observation = parse_x11_probe(
            b"PAM_STATUS=ready\r\nLASPER_X11_PROJECTION_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nUSERNS_DEVICE=4\r\nUSERNS_INODE=5\r\nMOUNT=MISSING\r\nCLIENT=MISSING\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1001));
    }

    #[test]
    fn unknown_field_error_identifies_the_bounded_escaped_key() {
        let error = parse_x11_probe(
            b"LASPER_X11_PROJECTION_PROBE_V1\nEUID=1000\nPAM_\x1b=unexpected\nRESULT=READY\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains(r"PAM_\x1b"));
    }

    #[test]
    fn request_keeps_both_paths_as_fixed_arguments() {
        let request = X11ProjectionProbeRequest::new(
            machine(),
            user(),
            Path::new("/mnt/x11/X0"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .unwrap();
        assert_eq!(request.path(), "/bin/sh");
        assert_eq!(request.args().len(), 6);
        assert_eq!(request.args()[4], "/mnt/x11/X0");
        assert_eq!(request.args()[5], "/tmp/.X11-unix/X0");
        assert!(X11ProjectionProbeRequest::new(
            machine(),
            user(),
            Path::new("../socket"),
            Path::new("/tmp/.X11-unix/X0")
        )
        .is_err());
    }
}
