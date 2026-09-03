use crate::application::sessions::{ObservedGuestIdentity, SessionError, TerminalSessionHandle};
use std::time::Duration;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_PROBE_LINE_BYTES: usize = 1024;
const PROBE_MAGIC: &[u8] = b"LASPER_WAYLAND_PROBE_V1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaylandTargetAccess {
    Unchecked,
    Accessible,
    Missing,
    Denied,
    NotSocket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaylandProbeObservation {
    pub(crate) identity: ObservedGuestIdentity,
    pub(crate) target: WaylandTargetAccess,
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
        } else if line == b"RESULT=READY" {
            ready = true;
        } else {
            return Err(SessionError::new("Wayland probe returned an unknown field"));
        }
    }

    match (uid, gid, target, ready) {
        (Some(uid), Some(gid), Some(target), true) => Ok(WaylandProbeObservation {
            identity: ObservedGuestIdentity::new(uid, gid),
            target,
        }),
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
        b"UNCHECKED" => Ok(WaylandTargetAccess::Unchecked),
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

    #[test]
    fn parses_identity_and_target_after_bounded_pam_noise() {
        let observation = parse_wayland_probe(
            b"Last login: today\r\nLASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nTARGET=ACCESSIBLE\r\nRESULT=READY\r\n",
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
            b"PAM notice: \x1b[0mLASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nTARGET=ACCESSIBLE\r\nRESULT=READY\r\n",
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
    fn parses_identity_only_result_explicitly() {
        let observation = parse_wayland_probe(
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1000\nEGID=1000\nTARGET=UNCHECKED\nRESULT=READY\n",
        )
        .unwrap();

        assert_eq!(observation.target, WaylandTargetAccess::Unchecked);
    }

    #[test]
    fn parses_the_reported_crlf_identity_frame() {
        let observation = parse_wayland_probe(
            b"LASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nTARGET=UNCHECKED\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1000));
        assert_eq!(observation.target, WaylandTargetAccess::Unchecked);
    }

    #[test]
    fn rejects_duplicate_unknown_and_incomplete_protocol_fields() {
        for output in [
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEUID=2\nEGID=3\nTARGET=UNCHECKED\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nPATH=/tmp\nTARGET=UNCHECKED\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nRESULT=READY\n".as_slice(),
            b"EUID=1\nEGID=3\nTARGET=UNCHECKED\nRESULT=READY\n".as_slice(),
        ] {
            assert!(parse_wayland_probe(output).is_err());
        }
    }
}
