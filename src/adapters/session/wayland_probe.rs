//! Fixed Wayland identity/access probe using the shared guest probe core.

use std::path::Path;

use crate::application::sessions::{
    ObservedGuestIdentity, SessionError, TerminalSessionHandle, ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;

use super::probe::{
    guest_script, validate_absolute_path, ProbePathError, ProbeProtocol, SocketObservation,
};

const PROTOCOL: ProbeProtocol = ProbeProtocol {
    magic: "LASPER_WAYLAND_PROBE_V1",
    label: "Wayland probe",
    maximum_output: 8 * 1024,
};

const WAYLAND_PROBE_BODY: &str = r#"if ! probe_guest_identity; then
    probe_failure 'guest identity could not be read'
    exit 0
fi
probe_target=$(probe_socket TARGET "$1" DEVICE INODE)
printf '%s\n' \
    "$probe_magic" \
    "EUID=$probe_euid" \
    "EGID=$probe_egid" \
    "$probe_target" \
    'RESULT=READY'
"#;

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
    ) -> Result<Self, ProbePathError> {
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

    pub(crate) fn path(&self) -> &'static str {
        "/bin/sh"
    }

    pub(crate) fn args(&self) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            guest_script(PROTOCOL.magic, WAYLAND_PROBE_BODY),
            "lasper-wayland-probe".into(),
            self.target.clone(),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaylandProbeObservation {
    pub(crate) identity: ObservedGuestIdentity,
    pub(crate) target: SocketObservation,
}

pub(crate) async fn collect_wayland_probe(
    handle: &mut TerminalSessionHandle,
) -> Result<WaylandProbeObservation, SessionError> {
    parse_wayland_probe(&PROTOCOL.collect(handle).await?)
}

fn parse_wayland_probe(bytes: &[u8]) -> Result<WaylandProbeObservation, SessionError> {
    let mut frame = PROTOCOL.parse(bytes)?;
    let identity = ObservedGuestIdentity::new(frame.u32(b"EUID")?, frame.u32(b"EGID")?);
    let target = SocketObservation::parse(&mut frame, b"TARGET", b"DEVICE", b"INODE")?;
    frame.finish()?;
    Ok(WaylandProbeObservation { identity, target })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::probe::SocketAccess;
    use crate::application::sessions::terminal_session_channel;
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

    #[tokio::test]
    async fn collection_finishes_when_the_result_frame_arrives() {
        let (mut handle, endpoint) =
            terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
        endpoint
            .output
            .send(
                b"LASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nTARGET=UNAVAILABLE\r\nRESULT=READY\r\n"
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
        assert_eq!(observation.target.access, SocketAccess::Unavailable);
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
        assert_eq!(observation.target.access, SocketAccess::Accessible);
        assert_eq!(
            observation.target.identity,
            Some((metadata.dev(), metadata.ino()))
        );
        assert_eq!(observation.identity.uid(), uzers::get_effective_uid());
    }

    #[test]
    fn failed_socket_stat_still_emits_a_complete_frame() {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let request = WaylandProbeRequest::target(machine(), user(), &path).unwrap();
        let mut args = request.args();
        // The socket exists and is writable, but its metadata becomes
        // unavailable at the syscall boundary (for example during an unmount).
        args[2] = format!("stat() {{ return 1; }}\n{}", args[2]);
        let output = std::process::Command::new(request.path())
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(output.status.success());
        let observed = parse_wayland_probe(&output.stdout).unwrap();
        assert_eq!(observed.target.access, SocketAccess::Unavailable);
        assert_eq!(observed.target.identity, None);
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
        assert_eq!(observation.target.access, SocketAccess::Accessible);
    }

    #[test]
    fn finds_the_last_protocol_frame_after_unterminated_terminal_noise() {
        let observation = parse_wayland_probe(
            b"PAM notice: \x1b[0mLASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nTARGET=ACCESSIBLE\r\nDEVICE=69\r\nINODE=100\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1001));
        assert_eq!(observation.target.access, SocketAccess::Accessible);
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
            b"LASPER_WAYLAND_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nTARGET=UNAVAILABLE\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1000));
        assert_eq!(observation.target.access, SocketAccess::Unavailable);
    }

    #[test]
    fn rejects_duplicate_unknown_and_incomplete_protocol_fields() {
        for output in [
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEUID=2\nEGID=3\nTARGET=UNAVAILABLE\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nPATH=/tmp\nTARGET=UNAVAILABLE\nRESULT=READY\n"
                .as_slice(),
            b"LASPER_WAYLAND_PROBE_V1\nEUID=1\nEGID=3\nRESULT=READY\n".as_slice(),
            b"EUID=1\nEGID=3\nTARGET=UNAVAILABLE\nRESULT=READY\n".as_slice(),
        ] {
            assert!(parse_wayland_probe(output).is_err());
        }
    }
}
