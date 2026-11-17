use super::fixture::{command, final_exit, protocol, ready, reply, Server, BROKER, PREPARATION};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::drive_io;
use lasper_guest_host_transport::{open_shell, ShellOpenError, StartError, StartOutcome};
use lasper_guest_protocol::{
    ErrorFrame, ExecutionKind, ExitReason, GuestErrorCode, GuestOperation, GuestProcess,
    PreparationControlKind, PrepareRequest, ProcessEnvironment, TerminalSize,
};
use std::{
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    time::Duration,
};
use tokio::io::AsyncReadExt;

#[derive(Clone, Copy)]
enum StartResponse {
    Exit,
    Reject,
    Withhold,
}

#[tokio::test]
async fn failed_shell_open_preserves_terminal_diagnostics_and_the_control_owner() {
    for response in [
        StartResponse::Exit,
        StartResponse::Reject,
        StartResponse::Withhold,
    ] {
        let size = TerminalSize::new(80, 24).unwrap();
        let expected = PrepareRequest::new(
            "alice",
            ExecutionKind::Terminal,
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            Some(size),
        )
        .unwrap();
        let server = Server::spawn_for_request(expected, move |guest, request| {
            let worker = ready(&guest, &request, 45_000);
            reply(&guest, command(&guest, &request), 45_000);
            let mut bytes = [0; 4096];
            let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
            let start_id = frame.header().request_id();
            drop(frame);
            // Write from the slave, as a shell printing a startup error would.
            // SAFETY: TIOCGPTPEER opens this owned master's slave as a new FD.
            let slave = unsafe {
                libc::ioctl(
                    worker.terminal.as_ref().unwrap().as_raw_fd(),
                    libc::TIOCGPTPEER,
                    libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
                )
            };
            assert!(slave >= 0);
            // SAFETY: the ioctl returned a new descriptor with no other owner.
            File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) })
                .write_all(b"shell startup diagnostic")
                .unwrap();
            if !matches!(response, StartResponse::Exit) {
                if matches!(response, StartResponse::Withhold) {
                    let close = command(&guest, &request);
                    assert_eq!(close.kind(), PreparationControlKind::Close);
                    reply(&guest, close, 0);
                    final_exit(&guest, &request, ExitReason::ClientClosed);
                    assert!(receive_guest_frame(&guest, &mut bytes).is_err());
                    return;
                }
                let error = ErrorFrame::new(
                    start_id,
                    BROKER,
                    Some(PREPARATION),
                    GuestOperation::Start,
                    GuestErrorCode::InvalidState,
                    None,
                )
                .unwrap();
                let (header, length) = protocol().encode_error(&error, &mut bytes).unwrap();
                send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
                let close = command(&guest, &request);
                assert_eq!(close.kind(), PreparationControlKind::Close);
                reply(&guest, close, 0);
            }
            final_exit(&guest, &request, ExitReason::ClientClosed);
            assert!(receive_guest_frame(&guest, &mut bytes).is_err());
        });
        let client = server
            .client(lasper_guest_host_transport::ClientTimeouts {
                operation: Duration::from_millis(100),
                ..Default::default()
            })
            .await;
        let error = drive_io(open_shell(
            &client,
            "alice".into(),
            size,
            Default::default(),
        ))
        .await
        .expect_err("shell did not accept Start");
        let ShellOpenError::Start {
            outcome,
            session,
            mut pty,
        } = error
        else {
            panic!("Prepare should succeed");
        };
        match response {
            StartResponse::Exit => assert!(matches!(outcome, Ok(StartOutcome::Reported(_)))),
            StartResponse::Reject => assert!(matches!(outcome, Err(StartError::Rejected(_)))),
            StartResponse::Withhold => assert!(matches!(outcome, Ok(StartOutcome::Unknown { .. }))),
        }
        let (completion, output) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(session.close(), async {
                let mut output = [0; 24];
                pty.read_exact(&mut output).await.unwrap();
                output
            })
        })
        .await
        .unwrap();
        assert_eq!(&output, b"shell startup diagnostic");
        assert!(completion.report.unwrap().is_final());
        assert!(completion.error.is_none());
        drop((session, pty));
        server.finish().await;
    }
}
