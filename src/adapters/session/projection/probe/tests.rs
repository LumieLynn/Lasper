use super::*;
use crate::application::sessions::terminal_session_channel;
use crate::domain::session::{SessionId, TerminalAttachmentKind};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PROTOCOL: ProbeProtocol = ProbeProtocol {
    magic: "LASPER_TEST_PROBE",
    label: "test probe",
    maximum_output: 4 * 1024,
};

#[tokio::test]
async fn completed_frame_is_independent_of_chunk_boundaries_and_later_markers() {
    let frame = b"PAM notice: LASPER_TEST_PROBE\r\nVALUE=42\r\nRESULT=READY\r\n";
    let mut output = frame.to_vec();
    output.extend_from_slice(b"Disconnected\nLASPER_TEST_PROBE\nVALUE=99\n");
    for split in 0..=output.len() {
        let (mut handle, mut endpoint) =
            terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
        for chunk in [&output[..split], &output[split..]] {
            endpoint.output.send(chunk.to_vec()).await.unwrap();
        }
        let bytes = PROTOCOL.collect(&mut handle).await.unwrap();
        assert_eq!(bytes, frame, "split at {split}");
        let mut parsed = PROTOCOL.parse(&bytes).unwrap();
        assert_eq!(parsed.u32(b"VALUE").unwrap(), 42);
        parsed.finish().unwrap();
        endpoint.close.try_recv().unwrap();
    }
}

#[tokio::test]
async fn oversized_transport_postlude_does_not_invalidate_a_complete_frame() {
    let frame = b"LASPER_TEST_PROBE\nVALUE=42\nRESULT=READY\n";
    let mut output = frame.to_vec();
    output.extend(vec![b'x'; PROTOCOL.maximum_output]);
    assert!(PROTOCOL.parse(&output).is_ok());
    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
    endpoint.output.send(output).await.unwrap();
    assert_eq!(PROTOCOL.collect(&mut handle).await.unwrap(), frame);
    endpoint.close.try_recv().unwrap();
}

#[tokio::test]
async fn fragmented_frame_uses_the_same_boundary_for_collection_and_parsing() {
    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
    for chunk in [
        b"PAM_NOTICE=ready\r\nLASPER_TEST_".as_slice(),
        b"PROBE\r\nVALUE=42\r\nRESULT=REA".as_slice(),
        b"DY\r".as_slice(),
        b"\nConnection closed\r\nAFTER=transport output\r\n".as_slice(),
    ] {
        endpoint.output.send(chunk.to_vec()).await.unwrap();
    }

    let bytes = tokio::time::timeout(Duration::from_millis(100), PROTOCOL.collect(&mut handle))
        .await
        .expect("complete frame waited for transport EOF")
        .unwrap();
    let mut frame = PROTOCOL.parse(&bytes).unwrap();
    assert_eq!(frame.u32(b"VALUE").unwrap(), 42);
    frame.finish().unwrap();
    endpoint.close.try_recv().unwrap();
}

#[tokio::test]
async fn error_frame_finishes_without_waiting_for_transport_eof() {
    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
    endpoint
        .output
        .send(b"LASPER_TEST_PROBE\nERROR=namespace unavailable\nRESULT=ERROR\n".to_vec())
        .await
        .unwrap();

    let bytes = tokio::time::timeout(Duration::from_millis(100), PROTOCOL.collect(&mut handle))
        .await
        .unwrap()
        .unwrap();
    let error = PROTOCOL.parse(&bytes).err().unwrap();
    assert!(error.to_string().contains("namespace unavailable"));
    endpoint.close.try_recv().unwrap();
}

#[tokio::test(start_paused = true)]
async fn collection_deadline_is_not_extended_by_partial_output() {
    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
    endpoint
        .output
        .send(b"LASPER_TEST_PROBE\n".to_vec())
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let task = tokio::spawn(async move { PROTOCOL.collect(&mut handle).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    endpoint.output.send(b"VALUE=42\n".to_vec()).await.unwrap();
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;

    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    endpoint.close.try_recv().unwrap();
}

#[tokio::test]
async fn collection_closes_the_session_on_missing_output_or_byte_limit() {
    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
    drop(handle.take_output());
    assert!(PROTOCOL.collect(&mut handle).await.is_err());
    endpoint.close.try_recv().unwrap();

    let (mut handle, mut endpoint) =
        terminal_session_channel(SessionId::new(2).unwrap(), TerminalAttachmentKind::Login);
    endpoint
        .output
        .send(vec![b'x'; PROTOCOL.maximum_output + 1])
        .await
        .unwrap();
    let error = PROTOCOL.collect(&mut handle).await.unwrap_err();
    assert!(error.to_string().contains("output exceeded its limit"));
    endpoint.close.try_recv().unwrap();
}

#[test]
fn fields_are_unique_bounded_and_only_unsigned_decimal_numbers() {
    assert!(PROTOCOL
        .parse(b"LASPER_TEST_PROBE\nVALUE=1\nVALUE=2\nRESULT=READY\n")
        .is_err());
    for number in ["", "-1", "+1", "0xff", "18446744073709551616"] {
        let bytes = format!("LASPER_TEST_PROBE\nVALUE={number}\nRESULT=READY\n");
        let mut frame = PROTOCOL.parse(bytes.as_bytes()).unwrap();
        assert!(frame.u64(b"VALUE").is_err(), "accepted {number}");
    }
    let mut frame = PROTOCOL
        .parse(b"LASPER_TEST_PROBE\nVALUE=4294967296\nRESULT=READY\n")
        .unwrap();
    assert!(frame.u32(b"VALUE").is_err());
    let bytes = format!(
        "LASPER_TEST_PROBE\nVALUE={}\nRESULT=READY\n",
        "x".repeat(1024)
    );
    assert!(PROTOCOL.parse(bytes.as_bytes()).is_err());
}

#[test]
fn socket_identity_is_required_for_denied_and_rejected_for_unobserved_paths() {
    let mut frame = PROTOCOL
        .parse(b"LASPER_TEST_PROBE\nTARGET=DENIED\nDEVICE=1\nINODE=2\nRESULT=READY\n")
        .unwrap();
    let observed = SocketObservation::parse(&mut frame, b"TARGET", b"DEVICE", b"INODE").unwrap();
    assert_eq!(observed.access, SocketAccess::Denied);
    assert_eq!(observed.identity, Some((1, 2)));
    frame.finish().unwrap();

    for fields in [
        "TARGET=DENIED\n",
        "TARGET=DENIED\nDEVICE=1\n",
        "TARGET=UNAVAILABLE\nDEVICE=1\nINODE=2\n",
        "TARGET=NOT_SOCKET\nDEVICE=1\nINODE=2\n",
    ] {
        let bytes = format!("LASPER_TEST_PROBE\n{fields}RESULT=READY\n");
        let mut frame = PROTOCOL.parse(bytes.as_bytes()).unwrap();
        assert!(SocketObservation::parse(&mut frame, b"TARGET", b"DEVICE", b"INODE").is_err());
    }
}

fn socket_command(path: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        &guest_script(
            PROTOCOL.magic,
            "printf '%s\\n' \"$probe_magic\"\nprobe_socket TARGET \"$1\" DEVICE INODE\nprintf '%s\\n' RESULT=READY",
        ),
        "lasper-test-probe",
    ]);
    command.arg(path);
    if uzers::get_effective_uid() == 0 {
        command.uid(65534).gid(65534);
    }
    command
}

fn observe_socket(path: &Path) -> SocketObservation {
    let output = socket_command(path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut frame = PROTOCOL.parse(&output.stdout).unwrap();
    let observed = SocketObservation::parse(&mut frame, b"TARGET", b"DEVICE", b"INODE").unwrap();
    frame.finish().unwrap();
    observed
}

#[test]
fn unsearchable_parent_is_unavailable_rather_than_missing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let observed = observe_socket(&path);
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(observed.access, SocketAccess::Unavailable);
    assert_eq!(observed.identity, None);
}

#[test]
fn denied_socket_keeps_the_identity_read_by_stat() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = directory.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o111)).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let observed = observe_socket(&path);
    assert_eq!(observed.access, SocketAccess::Denied);
    assert_eq!(observed.identity, Some((metadata.dev(), metadata.ino())));
}

#[test]
fn absent_socket_is_unavailable_and_regular_file_is_not_a_socket() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = directory.path().join("file");
    assert_eq!(observe_socket(&path).access, SocketAccess::Unavailable);
    std::fs::write(&path, b"not a socket").unwrap();
    assert_eq!(observe_socket(&path).access, SocketAccess::NotSocket);
}
