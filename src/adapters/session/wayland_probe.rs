//! The fixed Wayland identity/access probe.
//!
//! This module owns both halves of the probe contract: the closed request
//! which a session transport has to execute and the bounded protocol parser
//! which consumes its output.  Neither half knows whether the request is
//! carried by machine1 D-Bus or by `machinectl`.

use crate::application::sessions::{
    ObservedGuestIdentity, SessionError, TerminalSessionHandle, ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use std::path::{Component, Path};
use std::time::Duration;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_PROBE_LINE_BYTES: usize = 1024;
const PROBE_MAGIC: &[u8] = b"LASPER_WAYLAND_PROBE_V1";
const MAX_TARGET_PATH_BYTES: usize = 4096;

const WAYLAND_PROBE_SCRIPT: &str = r#"euid=
egid=
while read -r key _real effective _rest; do
    case "$key" in
        Uid:) euid=$effective ;;
        Gid:) egid=$effective ;;
    esac
done < /proc/self/status
if [ ! -e "$1" ]; then
    target=MISSING
elif [ ! -S "$1" ]; then
    target=NOT_SOCKET
elif [ ! -w "$1" ]; then
    target=DENIED
else
    target=ACCESSIBLE
fi
printf '%s\n' \
    'LASPER_WAYLAND_PROBE_V1' \
    "EUID=$euid" \
    "EGID=$egid" \
    "TARGET=$target"
if [ "$target" = ACCESSIBLE ]; then
    LC_ALL=C stat -Lc 'DEVICE=%d
INODE=%i' -- "$1" || exit 1
fi
printf '%s\n' 'RESULT=READY'
"#;

/// A transport-neutral request for Lasper's fixed Wayland probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaylandProbeRequest {
    machine: MachineName,
    user: ValidatedGuestUserName,
    target: String,
}

impl WaylandProbeRequest {
    pub(crate) fn target(
        machine: MachineName,
        user: ValidatedGuestUserName,
        target: &Path,
    ) -> Result<Self, WaylandProbeRequestError> {
        Ok(Self {
            machine,
            user,
            target: validate_absolute_path(target)?,
        })
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub(crate) fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }

    /// The fixed executable passed to machine1's `OpenMachineShell` call.
    pub(crate) fn path(&self) -> &'static str {
        "/bin/sh"
    }

    /// The fixed argv, including the validated target as `$1`.
    pub(crate) fn args(&self) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            WAYLAND_PROBE_SCRIPT.into(),
            "lasper-wayland-probe".into(),
            self.target.clone(),
        ]
    }
}

fn validate_absolute_path(path: &Path) -> Result<String, WaylandProbeRequestError> {
    if !path.is_absolute() {
        return Err(WaylandProbeRequestError::NotAbsolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(WaylandProbeRequestError::RelativeComponent);
    }
    let value = path.to_str().ok_or(WaylandProbeRequestError::NonUtf8)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(WaylandProbeRequestError::InvalidValue);
    }
    if value.len() > MAX_TARGET_PATH_BYTES {
        return Err(WaylandProbeRequestError::TooLong {
            maximum: MAX_TARGET_PATH_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WaylandProbeRequestError {
    #[error("Wayland probe target must be an absolute path")]
    NotAbsolute,
    #[error("Wayland probe target must not contain relative path components")]
    RelativeComponent,
    #[error("Wayland probe target is not valid UTF-8")]
    NonUtf8,
    #[error("Wayland probe target contains an empty or control-character value")]
    InvalidValue,
    #[error("Wayland probe target exceeds the {maximum}-byte path limit")]
    TooLong { maximum: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaylandTargetAccess {
    Accessible,
    Missing,
    Denied,
    NotSocket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaylandProbeObservation {
    pub(crate) identity: ObservedGuestIdentity,
    pub(crate) target: WaylandTargetAccess,
    pub(crate) socket_identity: Option<(u64, u64)>,
}

pub(crate) async fn collect_wayland_probe(
    handle: &mut TerminalSessionHandle,
) -> Result<WaylandProbeObservation, SessionError> {
    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("Wayland probe output is unavailable"))?;
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut bytes = Vec::new();

    loop {
        match tokio::time::timeout_at(deadline, output.recv()).await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_PROBE_OUTPUT_BYTES {
                    handle.close();
                    return Err(SessionError::new("Wayland probe output exceeded its limit"));
                }
                bytes.extend_from_slice(&chunk);
                if let Some(frame_end) = complete_probe_frame_end(&bytes) {
                    handle.close();
                    return parse_wayland_probe(&bytes[..frame_end]);
                }
            }
            Ok(None) => break,
            Err(_) => {
                handle.close();
                return Err(SessionError::new("Wayland projection probe timed out"));
            }
        }
    }
    handle.close();
    parse_wayland_probe(&bytes)
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

fn parse_wayland_probe(bytes: &[u8]) -> Result<WaylandProbeObservation, SessionError> {
    if bytes.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(SessionError::new("Wayland probe output exceeded its limit"));
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

    let mut uid = None;
    let mut gid = None;
    let mut target = None;
    let mut device = None;
    let mut inode = None;
    let mut ready = false;
    for raw_line in framed.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.len() > MAX_PROBE_LINE_BYTES {
            return Err(SessionError::new("Wayland probe line exceeded its limit"));
        }
        if line.is_empty() {
            continue;
        }
        if ready {
            return Err(SessionError::new(
                "Wayland probe emitted data after its result",
            ));
        }
        if let Some(value) = line.strip_prefix(b"EUID=") {
            if uid.replace(parse_u32(value, "EUID")?).is_some() {
                return Err(SessionError::new("Wayland probe repeated EUID"));
            }
        } else if let Some(value) = line.strip_prefix(b"EGID=") {
            if gid.replace(parse_u32(value, "EGID")?).is_some() {
                return Err(SessionError::new("Wayland probe repeated EGID"));
            }
        } else if let Some(value) = line.strip_prefix(b"TARGET=") {
            if target.replace(parse_target(value)?).is_some() {
                return Err(SessionError::new("Wayland probe repeated TARGET"));
            }
        } else if let Some(value) = line.strip_prefix(b"DEVICE=") {
            if device.replace(parse_u64(value, "DEVICE")?).is_some() {
                return Err(SessionError::new("Wayland probe repeated DEVICE"));
            }
        } else if let Some(value) = line.strip_prefix(b"INODE=") {
            if inode.replace(parse_u64(value, "INODE")?).is_some() {
                return Err(SessionError::new("Wayland probe repeated INODE"));
            }
        } else if line == b"RESULT=READY" {
            ready = true;
        } else {
            return Err(SessionError::new("Wayland probe returned an unknown field"));
        }
    }

    match (uid, gid, target, ready) {
        (Some(uid), Some(gid), Some(target), true) => {
            let socket_identity = match (target, device, inode) {
                (WaylandTargetAccess::Accessible, Some(device), Some(inode)) => {
                    Some((device, inode))
                }
                (WaylandTargetAccess::Accessible, _, _) => {
                    return Err(incomplete_probe_error(
                        "accessible target is missing DEVICE or INODE",
                        bytes,
                    ));
                }
                (_, None, None) => None,
                _ => {
                    return Err(SessionError::new(
                        "Wayland probe returned socket identity for an inaccessible target",
                    ));
                }
            };
            Ok(WaylandProbeObservation {
                identity: ObservedGuestIdentity::new(uid, gid),
                target,
                socket_identity,
            })
        }
        _ => {
            let mut missing = Vec::new();
            if uid.is_none() {
                missing.push("EUID");
            }
            if gid.is_none() {
                missing.push("EGID");
            }
            if target.is_none() {
                missing.push("TARGET");
            }
            if !ready {
                missing.push("RESULT");
            }
            Err(incomplete_probe_error(
                &format!("missing {}", missing.join(", ")),
                bytes,
            ))
        }
    }
}

fn incomplete_probe_error(reason: &str, bytes: &[u8]) -> SessionError {
    SessionError::new(format!(
        "Wayland probe result was incomplete ({reason}; captured output: {})",
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

fn parse_target(value: &[u8]) -> Result<WaylandTargetAccess, SessionError> {
    match value {
        b"ACCESSIBLE" => Ok(WaylandTargetAccess::Accessible),
        b"MISSING" => Ok(WaylandTargetAccess::Missing),
        b"DENIED" => Ok(WaylandTargetAccess::Denied),
        b"NOT_SOCKET" => Ok(WaylandTargetAccess::NotSocket),
        _ => Err(SessionError::new(
            "Wayland probe returned an invalid TARGET",
        )),
    }
}

fn parse_u32(value: &[u8], field: &str) -> Result<u32, SessionError> {
    u32::try_from(parse_u64(value, field)?)
        .map_err(|_| SessionError::new(format!("Wayland probe returned an invalid {field}")))
}

fn parse_u64(value: &[u8], field: &str) -> Result<u64, SessionError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(SessionError::new(format!(
            "Wayland probe returned an invalid {field}"
        )));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| SessionError::new(format!("Wayland probe returned an invalid {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::terminal_session_channel;
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn collection_finishes_when_the_result_frame_arrives() {
        let (mut handle, endpoint) =
            terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
        endpoint
            .output
            .send(
                b"LASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nTARGET=MISSING\r\nRESULT=READY\r\n"
                    .to_vec(),
            )
            .await
            .unwrap();

        let observation = tokio::time::timeout(
            Duration::from_millis(100),
            collect_wayland_probe(&mut handle),
        )
        .await
        .expect("complete Wayland probe waited for the PTY to close")
        .unwrap();
        assert_eq!(observation.target, WaylandTargetAccess::Missing);
    }

    #[test]
    fn fixed_script_reports_the_actual_socket_identity() {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("custom.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let request = WaylandProbeRequest::target(machine(), user(), &path).unwrap();
        let output = std::process::Command::new(request.path())
            .args(&request.args()[1..])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let observation = parse_wayland_probe(&output.stdout).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(observation.target, WaylandTargetAccess::Accessible);
        assert_eq!(
            observation.socket_identity,
            Some((metadata.dev(), metadata.ino()))
        );
        assert_eq!(observation.identity.uid(), uzers::get_effective_uid());
    }

    #[test]
    fn socket_identity_fields_are_unsigned_unique_and_complete() {
        let prefix = "LASPER_WAYLAND_PROBE_V1\nEUID=1000\nEGID=1000\nTARGET=ACCESSIBLE\n";
        for fields in [
            "DEVICE=-1\nINODE=1\n",
            "DEVICE=1\nDEVICE=2\nINODE=1\n",
            "DEVICE=1\nINODE=1\nINODE=2\n",
        ] {
            assert!(
                parse_wayland_probe(format!("{prefix}{fields}RESULT=READY\n").as_bytes()).is_err()
            );
        }
        assert!(
            parse_wayland_probe(format!("{prefix}DEVICE=1\nRESULT=READY\n").as_bytes()).is_err()
        );
    }

    fn machine() -> MachineName {
        MachineName::new("demo").unwrap()
    }

    fn user() -> ValidatedGuestUserName {
        ValidatedGuestUserName::new("alice").unwrap()
    }

    #[test]
    fn request_has_a_fixed_invocation_and_bounded_target_argument() {
        let target = WaylandProbeRequest::target(
            machine(),
            user(),
            Path::new("/run/lasper/wayland/1000/wayland-0"),
        )
        .unwrap();
        assert_eq!(target.path(), "/bin/sh");
        assert_eq!(target.args().len(), 5);
        assert_eq!(&target.args()[..2], ["/bin/sh", "-c"]);
        assert!(target.args()[2].contains("/proc/self/status"));
        assert_eq!(
            target.args().last().map(String::as_str),
            Some("/run/lasper/wayland/1000/wayland-0")
        );
    }

    #[test]
    fn target_rejects_relative_components_and_control_values() {
        for path in ["wayland-0", "/run/../tmp/socket", "/run/socket\n"] {
            assert!(WaylandProbeRequest::target(machine(), user(), Path::new(path)).is_err());
        }
    }

    #[test]
    fn parses_identity_and_target_after_bounded_pam_noise() {
        let observation = parse_wayland_probe(
            b"Last login: today\r\nLASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nTARGET=ACCESSIBLE\r\nDEVICE=69\r\nINODE=100\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(
            (observation.identity.uid(), observation.identity.gid()),
            (1000, 1001)
        );
        assert_eq!(observation.target, WaylandTargetAccess::Accessible);
    }

    #[test]
    fn finds_the_last_protocol_frame_after_unterminated_terminal_noise() {
        let observation = parse_wayland_probe(
            b"PAM notice: \x1b[0mLASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nTARGET=ACCESSIBLE\r\nDEVICE=69\r\nINODE=100\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1001));
        assert_eq!(observation.target, WaylandTargetAccess::Accessible);
    }

    #[test]
    fn incomplete_result_reports_a_bounded_escaped_output_tail() {
        let error = parse_wayland_probe(b"shell error\r\n").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("protocol marker is missing"));
        assert!(message.contains(r"shell error\r\n"));
    }

    #[test]
    fn parses_a_crlf_probe_frame() {
        let observation = parse_wayland_probe(
            b"LASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nTARGET=MISSING\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1000));
        assert_eq!(observation.target, WaylandTargetAccess::Missing);
    }

    #[test]
    fn rejects_duplicate_unknown_and_incomplete_protocol_fields() {
        for output in [
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEUID=2\nEGID=3\nTARGET=MISSING\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nPATH=/tmp\nTARGET=MISSING\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nRESULT=READY\n".as_slice(),
            b"EUID=1\nEGID=3\nTARGET=MISSING\nRESULT=READY\n".as_slice(),
        ] {
            assert!(parse_wayland_probe(output).is_err());
        }
    }
}
