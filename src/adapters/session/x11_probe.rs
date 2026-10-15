//! Fixed guest-side observation used by the X11 projection resolver.
//!
//! Host UID mapping and X server ACL inspection stay outside the guest.
//! Framing, socket observations, and PTY collection use the shared probe core.

use std::path::Path;

use crate::application::sessions::{
    ObservedGuestIdentity, ObservedNamespaceIdentity, SessionError, TerminalSessionHandle,
    ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;

use super::probe::{
    guest_script, validate_absolute_path, ProbePathError, ProbeProtocol, SocketObservation,
};

const PROTOCOL: ProbeProtocol = ProbeProtocol {
    magic: "LASPER_X11_PROJECTION_PROBE_V1",
    label: "X11 projection probe",
    maximum_output: 12 * 1024,
};

const X11_PROBE_BODY: &str = r#"if ! probe_guest_identity; then
    probe_failure 'guest identity could not be read'
    exit 0
fi
if ! probe_userns=$(LC_ALL=C stat -Lc '%d:%i' -- /proc/self/ns/user 2>/dev/null); then
    probe_failure 'guest user namespace could not be inspected'
    exit 0
fi
probe_mount=$(probe_socket MOUNT "$1" MOUNT_DEVICE MOUNT_INODE)
probe_client=$(probe_socket CLIENT "$2" CLIENT_DEVICE CLIENT_INODE)
printf '%s\n' \
    "$probe_magic" \
    "EUID=$probe_euid" \
    "EGID=$probe_egid" \
    "USERNS_DEVICE=${probe_userns%%:*}" \
    "USERNS_INODE=${probe_userns#*:}" \
    "$probe_mount" \
    "$probe_client" \
    'RESULT=READY'
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
    ) -> Result<Self, ProbePathError> {
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
            guest_script(PROTOCOL.magic, X11_PROBE_BODY),
            "lasper-x11-projection-probe".into(),
            self.mount_target.clone(),
            self.client_path.clone(),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct X11ProjectionProbeObservation {
    pub(crate) identity: ObservedGuestIdentity,
    pub(crate) user_namespace: ObservedNamespaceIdentity,
    pub(crate) mount: SocketObservation,
    pub(crate) client: SocketObservation,
}

pub(crate) async fn collect_x11_probe(
    handle: &mut TerminalSessionHandle,
) -> Result<X11ProjectionProbeObservation, SessionError> {
    parse_x11_probe(&PROTOCOL.collect(handle).await?)
}

fn parse_x11_probe(bytes: &[u8]) -> Result<X11ProjectionProbeObservation, SessionError> {
    let mut frame = PROTOCOL.parse(bytes)?;
    let identity = ObservedGuestIdentity::new(frame.u32(b"EUID")?, frame.u32(b"EGID")?);
    let user_namespace =
        ObservedNamespaceIdentity::new(frame.u64(b"USERNS_DEVICE")?, frame.u64(b"USERNS_INODE")?);
    let mount = SocketObservation::parse(&mut frame, b"MOUNT", b"MOUNT_DEVICE", b"MOUNT_INODE")?;
    let client =
        SocketObservation::parse(&mut frame, b"CLIENT", b"CLIENT_DEVICE", b"CLIENT_INODE")?;
    frame.finish()?;
    Ok(X11ProjectionProbeObservation {
        identity,
        user_namespace,
        mount,
        client,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::probe::SocketAccess;
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

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
        assert_eq!(observation.mount.access, SocketAccess::Accessible);
        assert_eq!(observation.client.access, SocketAccess::Accessible);
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
            (0o100, SocketAccess::Denied),
            (0o500, SocketAccess::Denied),
            (0o200, SocketAccess::Accessible),
            (0o600, SocketAccess::Accessible),
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
                Ok(_) => assert_eq!(expected, SocketAccess::Accessible),
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                    assert_eq!(expected, SocketAccess::Denied);
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
            .send(b"LASPER_X11_PROJECTION_PROBE_V1\r\nEUID=1000\r\nEGID=1000\r\nUSERNS_DEVICE=4\r\nUSERNS_INODE=5\r\nMOUNT=UNAVAILABLE\r\nCLIENT=UNAVAILABLE\r\nRESULT=READY\r\n".to_vec())
            .await
            .unwrap();
        let observation =
            tokio::time::timeout(Duration::from_millis(100), collect_x11_probe(&mut handle))
                .await
                .expect("complete X11 probe waited for the PTY to close")
                .unwrap();
        assert_eq!(observation.mount.access, SocketAccess::Unavailable);
    }

    #[test]
    fn parser_rejects_missing_duplicate_and_inconsistent_fields() {
        let prefix = "LASPER_X11_PROJECTION_PROBE_V1\nEUID=1000\nEGID=1000\nUSERNS_DEVICE=4\nUSERNS_INODE=5\n";
        assert!(parse_x11_probe(
            format!("{prefix}MOUNT=ACCESSIBLE\nMOUNT_DEVICE=1\nCLIENT=UNAVAILABLE\nRESULT=READY\n")
                .as_bytes()
        )
        .is_err());
        assert!(parse_x11_probe(
            format!(
                "{prefix}MOUNT=UNAVAILABLE\nCLIENT=UNAVAILABLE\nCLIENT=UNAVAILABLE\nRESULT=READY\n"
            )
            .as_bytes()
        )
        .is_err());
        assert!(parse_x11_probe(
            format!(
                "{prefix}MOUNT=UNAVAILABLE\nMOUNT_DEVICE=1\nCLIENT=UNAVAILABLE\nRESULT=READY\n"
            )
            .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn parser_uses_the_last_frame_after_assignment_like_terminal_noise() {
        let observation = parse_x11_probe(
            b"PAM_STATUS=ready\r\nLASPER_X11_PROJECTION_PROBE_V1\r\nEUID=1000\r\nEGID=1001\r\nUSERNS_DEVICE=4\r\nUSERNS_INODE=5\r\nMOUNT=UNAVAILABLE\r\nCLIENT=UNAVAILABLE\r\nRESULT=READY\r\n",
        )
        .unwrap();

        assert_eq!(observation.identity, ObservedGuestIdentity::new(1000, 1001));
    }

    #[test]
    fn unknown_field_error_identifies_the_bounded_escaped_key() {
        let error = parse_x11_probe(
            b"LASPER_X11_PROJECTION_PROBE_V1\nEUID=1000\nEGID=1000\nUSERNS_DEVICE=4\nUSERNS_INODE=5\nMOUNT=UNAVAILABLE\nCLIENT=UNAVAILABLE\nPAM_\x1b=unexpected\nRESULT=READY\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains(r"PAM_\x1b"));
    }

    #[test]
    fn failed_socket_stat_still_emits_a_complete_frame() {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let request = X11ProjectionProbeRequest::new(machine(), user(), &path, &path).unwrap();
        let mut args = request.args();
        args[2] = format!(
            "probe_test_path=$1\nstat() {{\n    if [ \"$4\" = \"$probe_test_path\" ]; then return 1; fi\n    command stat \"$@\"\n}}\n{}",
            args[2]
        );
        let output = std::process::Command::new(request.path())
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(output.status.success());
        let observed = parse_x11_probe(&output.stdout).unwrap();
        assert_eq!(observed.mount.access, SocketAccess::Unavailable);
        assert_eq!(observed.client.access, SocketAccess::Unavailable);
        assert_eq!(observed.mount.identity, None);
        assert_eq!(observed.client.identity, None);
    }

    #[test]
    fn failed_namespace_stat_emits_an_explicit_error_frame() {
        let request = X11ProjectionProbeRequest::new(
            machine(),
            user(),
            Path::new("/mnt/x11/X0"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .unwrap();
        let mut args = request.args();
        args[2] = format!("stat() {{ return 1; }}\n{}", args[2]);
        let output = std::process::Command::new(request.path())
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(output.status.success());
        let error = parse_x11_probe(&output.stdout).unwrap_err();
        assert!(error
            .to_string()
            .contains("guest user namespace could not be inspected"));
        assert!(!error.to_string().contains("incomplete"));
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
